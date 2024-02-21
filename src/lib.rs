#![feature(allocator_api)]
#![feature(get_mut_unchecked)]

use std::sync::Arc;
use std::thread;

use ark_std::end_timer;
use ark_std::rand::rngs::OsRng;
use ark_std::start_timer;
use cuda::bn254::buffer_copy_with_shift;
use cuda::bn254::extended_prepare;
use cuda::bn254::field_mul;
use cuda::bn254::field_op;
use cuda::bn254::field_sum;
use cuda::bn254::FieldOp;
use device::cuda::CudaDeviceBufRaw;
use halo2_proofs::arithmetic::CurveAffine;
use halo2_proofs::arithmetic::Field;
use halo2_proofs::arithmetic::FieldExt;
use halo2_proofs::pairing::group::ff::BatchInvert as _;
use halo2_proofs::plonk::evaluation_gpu::Bop;
use halo2_proofs::plonk::evaluation_gpu::ProveExpression;
use halo2_proofs::plonk::evaluation_gpu::ProveExpressionUnit;
use halo2_proofs::plonk::Any;
use halo2_proofs::plonk::Expression;
use halo2_proofs::plonk::ProvingKey;
use halo2_proofs::poly::commitment::Params;
use halo2_proofs::transcript::EncodedChallenge;
use halo2_proofs::transcript::TranscriptWrite;
use hugetlb::HugePageAllocator;
use rayon::iter::IndexedParallelIterator as _;
use rayon::iter::IntoParallelIterator as _;
use rayon::iter::IntoParallelRefIterator as _;
use rayon::iter::IntoParallelRefMutIterator as _;
use rayon::iter::ParallelIterator as _;
use rayon::slice::ParallelSlice as _;

use crate::cuda::bn254::field_mul_sum_vec;
use crate::cuda::bn254::field_op_v2;
use crate::cuda::bn254::intt_raw;
use crate::cuda::bn254::msm;
use crate::cuda::bn254::msm_with_groups;
use crate::cuda::bn254::ntt;
use crate::cuda::bn254::ntt_prepare;
use crate::cuda::bn254::ntt_raw;
use crate::cuda::bn254::permutation_eval_h_l;
use crate::cuda::bn254::permutation_eval_h_p1;
use crate::cuda::bn254::permutation_eval_h_p2;
use crate::cuda::bn254::permutation_eval_h_r;
use crate::cuda::bn254::pick_from_buf;
use crate::device::cuda::CudaDevice;
use crate::device::Device as _;
use crate::device::DeviceResult;

mod cache;
pub mod cuda;
pub mod device;
mod hugetlb;

const ADD_RANDOM: bool = false;

pub fn prepare_advice_buffer<C: CurveAffine>(
    pk: &ProvingKey<C>,
) -> Vec<Vec<C::Scalar, HugePageAllocator>> {
    let rows = 1 << pk.get_vk().domain.k();
    let columns = pk.get_vk().cs.num_advice_columns;
    let zero = C::Scalar::zero();
    (0..columns)
        .into_par_iter()
        .map(|_| {
            let mut buf = Vec::new_in(HugePageAllocator);
            buf.resize(rows, zero);
            buf
        })
        .collect()
}

#[derive(Debug)]
pub enum Error {
    DeviceError(device::Error),
}

impl From<device::Error> for Error {
    fn from(e: device::Error) -> Self {
        Error::DeviceError(e)
    }
}

fn is_expression_pure_unit<F: FieldExt>(x: &Expression<F>) -> bool {
    x.is_constant().is_some()
        || x.is_pure_fixed().is_some()
        || x.is_pure_advice().is_some()
        || x.is_pure_instance().is_some()
}

fn lookup_classify<'a, 'b, C: CurveAffine, T>(
    pk: &'b ProvingKey<C>,
    lookups_buf: Vec<T>,
) -> [Vec<(usize, T)>; 3] {
    let mut single_unit_lookups = vec![];
    let mut single_comp_lookups = vec![];
    let mut tuple_lookups = vec![];

    pk.vk
        .cs
        .lookups
        .iter()
        .zip(lookups_buf.into_iter())
        .enumerate()
        .for_each(|(i, (lookup, buf))| {
            let is_single =
                lookup.input_expressions.len() == 1 && lookup.table_expressions.len() == 1;

            if is_single {
                let is_unit = is_expression_pure_unit(&lookup.input_expressions[0])
                    && is_expression_pure_unit(&lookup.table_expressions[0]);
                if is_unit {
                    single_unit_lookups.push((i, buf));
                } else {
                    single_comp_lookups.push((i, buf));
                }
            } else {
                tuple_lookups.push((i, buf))
            }
        });

    return [single_unit_lookups, single_comp_lookups, tuple_lookups];
}

fn handle_lookup_pair<F: FieldExt>(
    input: &Vec<F, HugePageAllocator>,
    table: &Vec<F, HugePageAllocator>,
    unusable_rows_start: usize,
) -> (Vec<F, HugePageAllocator>, Vec<F, HugePageAllocator>) {
    let compare = |a: &_, b: &_| unsafe {
        let a: &[u64; 4] = std::mem::transmute(a);
        let b: &[u64; 4] = std::mem::transmute(b);
        a.cmp(b)
    };

    let mut permuted_input = input.clone();
    let mut sorted_table = table.clone();

    permuted_input[0..unusable_rows_start].sort_unstable_by(compare);
    sorted_table[0..unusable_rows_start].sort_unstable_by(compare);

    let mut permuted_table_state = Vec::new_in(HugePageAllocator);
    permuted_table_state.resize(input.len(), false);

    let mut permuted_table = Vec::new_in(HugePageAllocator);
    permuted_table.resize(input.len(), F::zero());

    permuted_input
        .iter()
        .zip(permuted_table_state.iter_mut())
        .zip(permuted_table.iter_mut())
        .enumerate()
        .for_each(|(row, ((input_value, table_state), table_value))| {
            // If this is the first occurrence of `input_value` in the input expression
            if row == 0 || *input_value != permuted_input[row - 1] {
                *table_state = true;
                *table_value = *input_value;
            }
        });

    let to_next_unique = |i: &mut usize| {
        while *i < unusable_rows_start && !permuted_table_state[*i] {
            *i += 1;
        }
    };

    let mut i_unique_input_idx = 0;
    let mut i_sorted_table_idx = 0;
    for i in 0..unusable_rows_start {
        to_next_unique(&mut i_unique_input_idx);
        while i_unique_input_idx < unusable_rows_start
            && permuted_table[i_unique_input_idx] == sorted_table[i_sorted_table_idx]
        {
            i_unique_input_idx += 1;
            i_sorted_table_idx += 1;
            to_next_unique(&mut i_unique_input_idx);
        }
        if !permuted_table_state[i] {
            permuted_table[i] = sorted_table[i_sorted_table_idx];
            i_sorted_table_idx += 1;
        }
    }

    if true {
        let mut last = None;
        for (a, b) in permuted_input
            .iter()
            .zip(permuted_table.iter())
            .take(unusable_rows_start)
        {
            if *a != *b {
                assert_eq!(*a, last.unwrap());
            }
            last = Some(*a);
        }
    }

    if ADD_RANDOM {
        for cell in &mut permuted_input[unusable_rows_start..] {
            *cell = F::random(&mut OsRng);
        }
        for cell in &mut permuted_table[unusable_rows_start..] {
            *cell = F::random(&mut OsRng);
        }
    }

    (permuted_input, permuted_table)
}

/// Simple evaluation of an expression
pub fn evaluate_expr<F: FieldExt>(
    expression: &Expression<F>,
    size: usize,
    rot_scale: i32,
    fixed: &[&[F]],
    advice: &[&[F]],
    instance: &[&[F]],
    res: &mut [F],
) {
    let isize = size as i32;

    let get_rotation_idx = |idx: usize, rot: i32, rot_scale: i32, isize: i32| -> usize {
        (((idx as i32) + (rot * rot_scale)).rem_euclid(isize)) as usize
    };

    for (idx, value) in res.iter_mut().enumerate() {
        *value = expression.evaluate(
            &|scalar| scalar,
            &|_| panic!("virtual selectors are removed during optimization"),
            &|_, column_index, rotation| {
                fixed[column_index][get_rotation_idx(idx, rotation.0, rot_scale, isize)]
            },
            &|_, column_index, rotation| {
                advice[column_index][get_rotation_idx(idx, rotation.0, rot_scale, isize)]
            },
            &|_, column_index, rotation| {
                instance[column_index][get_rotation_idx(idx, rotation.0, rot_scale, isize)]
            },
            &|a| -a,
            &|a, b| a + &b,
            &|a, b| {
                let a = a();

                if a == F::zero() {
                    a
                } else {
                    a * b()
                }
            },
            &|a, scalar| a * scalar,
        );
    }
}

/// Simple evaluation of an expression
pub fn evaluate_exprs<F: FieldExt>(
    expressions: &[Expression<F>],
    size: usize,
    rot_scale: i32,
    fixed: &[&[F]],
    advice: &[&[F]],
    instance: &[&[F]],
    theta: F,
    res: &mut [F],
) {
    let isize = size as i32;
    let get_rotation_idx = |idx: usize, rot: i32, rot_scale: i32, isize: i32| -> usize {
        (((idx as i32) + (rot * rot_scale)).rem_euclid(isize)) as usize
    };
    for (idx, value) in res.iter_mut().enumerate() {
        for expression in expressions {
            *value = *value * theta;
            *value += expression.evaluate(
                &|scalar| scalar,
                &|_| panic!("virtual selectors are removed during optimization"),
                &|_, column_index, rotation| {
                    fixed[column_index][get_rotation_idx(idx, rotation.0, rot_scale, isize)]
                },
                &|_, column_index, rotation| {
                    advice[column_index][get_rotation_idx(idx, rotation.0, rot_scale, isize)]
                },
                &|_, column_index, rotation| {
                    instance[column_index][get_rotation_idx(idx, rotation.0, rot_scale, isize)]
                },
                &|a| -a,
                &|a, b| a + &b,
                &|a, b| {
                    let a = a();
                    if a == F::zero() {
                        a
                    } else {
                        a * b()
                    }
                },
                &|a, scalar| a * scalar,
            );
        }
    }
}

pub fn create_proof_from_advices<
    C: CurveAffine,
    E: EncodedChallenge<C>,
    T: TranscriptWrite<C, E>,
>(
    params: &Params<C>,
    pk: Arc<ProvingKey<C>>,
    instances: &[&[C::Scalar]],
    mut advices: Arc<Vec<Vec<C::Scalar, HugePageAllocator>>>,
    transcript: &mut T,
) -> Result<(), Error> {
    let k = pk.get_vk().domain.k() as usize;
    let size = 1 << pk.get_vk().domain.k();
    let extended_k = pk.get_vk().domain.extended_k() as usize;
    let rot_scale = 1 << (extended_k - k);
    let meta = &pk.vk.cs;
    let unusable_rows_start = params.n as usize - (meta.blinding_factors() + 1);
    let omega = pk.get_vk().domain.get_omega();

    let timer = start_timer!(|| "create single instances");
    let instance =
        halo2_proofs::plonk::create_single_instances(params, &pk, &[instances], transcript)
            .unwrap();
    let instance = Arc::new(instance);
    end_timer!(timer);

    let device = CudaDevice::get_device(0).unwrap();

    let timer = start_timer!(|| "pin advice memory to gpu");
    unsafe { Arc::get_mut_unchecked(&mut advices) }
        .iter_mut()
        .map(|x| -> Result<(), Error> {
            device.pin_memory(&mut x[..])?;
            Ok(())
        })
        .collect::<Result<_, _>>()?;
    end_timer!(timer);

    // add random value
    if ADD_RANDOM {
        let named = &pk.vk.cs.named_advices;
        unsafe { Arc::get_mut_unchecked(&mut advices) }
            .par_iter_mut()
            .enumerate()
            .for_each(|(i, advice)| {
                if named.iter().find(|n| n.1 as usize == i).is_none() {
                    for cell in &mut advice[unusable_rows_start..] {
                        *cell = C::Scalar::random(&mut OsRng);
                    }
                }
            });
    }

    let timer = start_timer!(|| format!("copy advice columns to gpu, count {}", advices.len()));
    let advices_device_buf = advices
        .iter()
        .map(|x| device.alloc_device_buffer_from_slice(x))
        .collect::<DeviceResult<Vec<_>>>()?;
    end_timer!(timer);

    /*
    let timer =
        start_timer!(|| format!("copy fixed columns to gpu, count {}", pk.fixed_values.len()));
    let fixed_device_buf = pk
        .fixed_values
        .iter()
        .map(|x| device.alloc_device_buffer_from_slice(x))
        .collect::<DeviceResult<Vec<_>>>()?;
    end_timer!(timer);
    */

    let timer = start_timer!(|| "copy g_lagrange buffer");
    let g_lagrange_buf = device
        .alloc_device_buffer_from_slice(&params.g_lagrange[..])
        .unwrap();
    end_timer!(timer);

    // thread for part of lookups
    let sub_pk = pk.clone();
    let sub_advices = advices.clone();
    let sub_instance = instance.clone();
    let lookup_handler = thread::spawn(move || {
        let pk = sub_pk;
        let advices = sub_advices;
        let instance = sub_instance;
        let timer =
            start_timer!(|| format!("prepare lookup buffer, count {}", pk.vk.cs.lookups.len()));
        let lookups = pk
            .vk
            .cs
            .lookups
            .par_iter()
            .map(|_| {
                let mut permuted_input = Vec::new_in(HugePageAllocator);
                permuted_input.resize(size, C::ScalarExt::zero());
                let mut permuted_table = Vec::new_in(HugePageAllocator);
                permuted_table.resize(size, C::ScalarExt::zero());
                let mut z = Vec::new_in(HugePageAllocator);
                z.resize(size, C::ScalarExt::zero());
                (permuted_input, permuted_table, z)
            })
            .collect::<Vec<_>>();
        end_timer!(timer);

        let [single_unit_lookups, single_comp_lookups, tuple_lookups] =
            lookup_classify(&pk, lookups);

        //let timer = start_timer!(|| format!("permute lookup unit {}", single_unit_lookups.len()));
        let single_unit_lookups = single_unit_lookups
            .into_par_iter()
            .map(|(i, (mut input, mut table, z))| {
                let f = |expr: &Expression<_>, target: &mut [_]| {
                    if let Some(v) = expr.is_constant() {
                        target.fill(v);
                    } else if let Some(idx) = expr.is_pure_fixed() {
                        target
                            .clone_from_slice(&pk.fixed_values[idx].values[0..unusable_rows_start]);
                    } else if let Some(idx) = expr.is_pure_instance() {
                        target.clone_from_slice(
                            &instance[0].instance_values[idx].values[0..unusable_rows_start],
                        );
                    } else if let Some(idx) = expr.is_pure_advice() {
                        target.clone_from_slice(&advices[idx][0..unusable_rows_start]);
                    } else {
                        unreachable!()
                    }
                };

                f(
                    &pk.vk.cs.lookups[i].input_expressions[0],
                    &mut input[0..unusable_rows_start],
                );
                f(
                    &pk.vk.cs.lookups[i].table_expressions[0],
                    &mut table[0..unusable_rows_start],
                );
                let (permuted_input, permuted_table) =
                    handle_lookup_pair(&input, &table, unusable_rows_start);
                (i, (permuted_input, permuted_table, input, table, z))
            })
            .collect::<Vec<_>>();
        //end_timer!(timer);

        let fixed_ref = &pk.fixed_values.iter().map(|x| &x[..]).collect::<Vec<_>>()[..];
        let advice_ref = &advices.iter().map(|x| &x[..]).collect::<Vec<_>>()[..];
        let instance_ref = &instance[0]
            .instance_values
            .iter()
            .map(|x| &x[..])
            .collect::<Vec<_>>()[..];

        let timer = start_timer!(|| format!("permute lookup comp {}", single_comp_lookups.len()));
        let single_comp_lookups = single_comp_lookups
            .into_par_iter()
            .map(|(i, (mut input, mut table, z))| {
                let f = |expr: &Expression<_>, target: &mut [_]| {
                    evaluate_expr(expr, size, 1, fixed_ref, advice_ref, instance_ref, target)
                };

                f(
                    &pk.vk.cs.lookups[i].input_expressions[0],
                    &mut input[0..unusable_rows_start],
                );
                f(
                    &pk.vk.cs.lookups[i].table_expressions[0],
                    &mut table[0..unusable_rows_start],
                );
                let (permuted_input, permuted_table) =
                    handle_lookup_pair(&input, &table, unusable_rows_start);
                (i, (permuted_input, permuted_table, input, table, z))
            })
            .collect::<Vec<_>>();
        end_timer!(timer);

        (single_unit_lookups, single_comp_lookups, tuple_lookups)
    });

    // Advice MSM
    let timer = start_timer!(|| format!("advices msm {}", advices_device_buf.len()));
    for s_buf in advices_device_buf {
        let commitment = msm(&device, &g_lagrange_buf, &s_buf, size)?;
        transcript.write_point(commitment).unwrap();
    }
    end_timer!(timer);

    let theta: C::ScalarExt = *transcript.squeeze_challenge_scalar::<()>();
    println!("theta is {:?}", theta);

    let timer = start_timer!(|| "wait single lookups");
    let (mut single_unit_lookups, mut single_comp_lookups, tuple_lookups) =
        lookup_handler.join().unwrap();
    end_timer!(timer);

    // After theta
    let sub_pk = pk.clone();
    let sub_advices = advices.clone();
    let sub_instance = instance.clone();
    let tuple_lookup_handler = thread::spawn(move || {
        let pk = sub_pk;
        let advices = sub_advices;
        let instance = sub_instance;
        //let timer = start_timer!(|| format!("permute lookup tuple {}", tuple_lookups.len()));

        let fixed_ref = &pk.fixed_values.iter().map(|x| &x[..]).collect::<Vec<_>>()[..];
        let advice_ref = &advices.iter().map(|x| &x[..]).collect::<Vec<_>>()[..];
        let instance_ref = &instance[0]
            .instance_values
            .iter()
            .map(|x| &x[..])
            .collect::<Vec<_>>()[..];

        let tuple_lookups = tuple_lookups
            .into_par_iter()
            .map(|(i, (mut input, mut table, z))| {
                let f = |expr: &[Expression<_>], target: &mut [_]| {
                    evaluate_exprs(
                        expr,
                        size,
                        1,
                        fixed_ref,
                        advice_ref,
                        instance_ref,
                        theta,
                        target,
                    )
                };

                f(
                    &pk.vk.cs.lookups[i].input_expressions[..],
                    &mut input[0..unusable_rows_start],
                );
                f(
                    &pk.vk.cs.lookups[i].table_expressions[..],
                    &mut table[0..unusable_rows_start],
                );
                let (permuted_input, permuted_table) =
                    handle_lookup_pair(&input, &table, unusable_rows_start);
                (i, (permuted_input, permuted_table, input, table, z))
            })
            .collect::<Vec<_>>();
        //end_timer!(timer);

        tuple_lookups
    });

    let mut lookup_permuted_commitments = vec![C::identity(); pk.vk.cs.lookups.len() * 2];

    let timer = start_timer!(|| format!(
        "single lookup msm {} {}",
        single_unit_lookups.len(),
        single_comp_lookups.len()
    ));
    for (i, (permuted_input, permuted_table, _, _, _)) in single_unit_lookups.iter() {
        let permuted_input_buf = device.alloc_device_buffer_from_slice(&permuted_input[..])?;
        let permuted_table_buf = device.alloc_device_buffer_from_slice(&permuted_table[..])?;
        lookup_permuted_commitments[i * 2] =
            msm(&device, &g_lagrange_buf, &permuted_input_buf, size)?;
        lookup_permuted_commitments[i * 2 + 1] =
            msm(&device, &g_lagrange_buf, &permuted_table_buf, size)?;
    }
    for (i, (permuted_input, permuted_table, _, _, _)) in single_comp_lookups.iter() {
        let permuted_input_buf = device.alloc_device_buffer_from_slice(&permuted_input[..])?;
        let permuted_table_buf = device.alloc_device_buffer_from_slice(&permuted_table[..])?;
        lookup_permuted_commitments[i * 2] =
            msm(&device, &g_lagrange_buf, &permuted_input_buf, size)?;
        lookup_permuted_commitments[i * 2 + 1] =
            msm(&device, &g_lagrange_buf, &permuted_table_buf, size)?;
    }
    end_timer!(timer);

    let timer = start_timer!(|| "wait tuple lookup");
    let mut tuple_lookups = tuple_lookup_handler.join().unwrap();
    end_timer!(timer);

    let timer = start_timer!(|| format!("tuple lookup msm {}", tuple_lookups.len(),));
    for (i, (permuted_input, permuted_table, _, _, _)) in tuple_lookups.iter() {
        let permuted_input_buf = device.alloc_device_buffer_from_slice(&permuted_input[..])?;
        let permuted_table_buf = device.alloc_device_buffer_from_slice(&permuted_table[..])?;
        lookup_permuted_commitments[i * 2] =
            msm(&device, &g_lagrange_buf, &permuted_input_buf, size)?;
        lookup_permuted_commitments[i * 2 + 1] =
            msm(&device, &g_lagrange_buf, &permuted_table_buf, size)?;
    }
    end_timer!(timer);

    for commitment in lookup_permuted_commitments.into_iter() {
        transcript.write_point(commitment).unwrap();
    }

    let beta: C::ScalarExt = *transcript.squeeze_challenge_scalar::<()>();
    println!("beta is {:?}", beta);
    let gamma: C::ScalarExt = *transcript.squeeze_challenge_scalar::<()>();
    println!("gamma is {:?}", gamma);

    let mut lookups = vec![];
    lookups.append(&mut single_unit_lookups);
    lookups.append(&mut single_comp_lookups);
    lookups.append(&mut tuple_lookups);
    lookups.sort_by(|l, r| usize::cmp(&l.0, &r.0));

    let timer = start_timer!(|| "generate lookup z");
    lookups
        .par_iter_mut()
        .for_each(|(_, (permuted_input, permuted_table, input, table, z))| {
            for ((z, permuted_input_value), permuted_table_value) in z
                .iter_mut()
                .zip(permuted_input.iter())
                .zip(permuted_table.iter())
            {
                *z = (beta + permuted_input_value) * &(gamma + permuted_table_value);
            }

            z.batch_invert();

            for ((z, input_value), table_value) in z.iter_mut().zip(input.iter()).zip(table.iter())
            {
                *z *= (beta + input_value) * &(gamma + table_value);
            }

            let mut tmp = C::ScalarExt::one();
            for i in 0..=unusable_rows_start {
                std::mem::swap(&mut tmp, &mut z[i]);
                tmp = tmp * z[i];
            }

            if ADD_RANDOM {
                for cell in &mut z[unusable_rows_start + 1..] {
                    *cell = C::Scalar::random(&mut OsRng);
                }
            } else {
                for cell in &mut z[unusable_rows_start + 1..] {
                    *cell = C::Scalar::zero();
                }
            }
        });

    let mut lookups = lookups
        .into_iter()
        .map(|(_, (permuted_input, permuted_table, _, _, z))| (permuted_input, permuted_table, z))
        .collect::<Vec<_>>();
    end_timer!(timer);

    let timer = start_timer!(|| "prepare ntt");
    let (intt_omegas_buf, intt_pq_buf) =
        ntt_prepare(&device, pk.get_vk().domain.get_omega_inv(), k)?;
    let divisor_buf = device
        .alloc_device_buffer_from_slice::<C::ScalarExt>(&[pk.get_vk().domain.ifft_divisor])?;
    end_timer!(timer);

    let chunk_len = &pk.vk.cs.degree() - 2;

    let timer = start_timer!(|| format!(
        "product permutation {}",
        (&pk).vk.cs.permutation.columns.chunks(chunk_len).len()
    ));

    let sub_pk = pk.clone();
    let sub_advices = advices.clone();
    let sub_instance = instance.clone();
    let permutation_products_handler = thread::spawn(move || {
        let pk = sub_pk;
        let advices = sub_advices;
        let instance = sub_instance;

        let fixed_ref = &pk.fixed_values.iter().map(|x| &x[..]).collect::<Vec<_>>()[..];
        let advice_ref = &advices.iter().map(|x| &x[..]).collect::<Vec<_>>()[..];
        let instance_ref = &instance[0]
            .instance_values
            .iter()
            .map(|x| &x[..])
            .collect::<Vec<_>>()[..];
        let mut p_z = pk
            .vk
            .cs
            .permutation
            .columns
            .par_chunks(chunk_len)
            .zip((&pk).permutation.permutations.par_chunks(chunk_len))
            .enumerate()
            .map(|(i, (columns, permutations))| {
                let mut delta_omega = C::Scalar::DELTA.pow_vartime([i as u64 * chunk_len as u64]);

                let mut modified_values = Vec::new_in(HugePageAllocator);
                modified_values.resize(size, C::ScalarExt::one());

                // Iterate over each column of the permutation
                for (&column, permuted_column_values) in columns.iter().zip(permutations.iter()) {
                    let values = match column.column_type() {
                        Any::Advice => advice_ref,
                        Any::Fixed => fixed_ref,
                        Any::Instance => instance_ref,
                    };
                    for i in 0..size as usize {
                        modified_values[i] *= &(beta * permuted_column_values[i]
                            + &gamma
                            + values[column.index()][i]);
                    }
                }

                // Invert to obtain the denominator for the permutation product polynomial
                modified_values.iter_mut().batch_invert();

                // Iterate over each column again, this time finishing the computation
                // of the entire fraction by computing the numerators
                for &column in columns.iter() {
                    let values = match column.column_type() {
                        Any::Advice => advice_ref,
                        Any::Fixed => fixed_ref,
                        Any::Instance => instance_ref,
                    };
                    for i in 0..size as usize {
                        modified_values[i] *=
                            &(delta_omega * &beta + &gamma + values[column.index()][i]);
                        delta_omega *= &omega;
                    }
                    delta_omega *= &C::Scalar::DELTA;
                }

                modified_values
            })
            .collect::<Vec<_>>();

        let mut tmp = C::ScalarExt::one();
        for z in p_z.iter_mut() {
            for i in 0..size {
                std::mem::swap(&mut tmp, &mut z[i]);
                tmp = tmp * z[i];
            }

            tmp = z[unusable_rows_start];

            for v in z[unusable_rows_start + 1..].iter_mut() {
                if ADD_RANDOM {
                    *v = C::Scalar::random(&mut OsRng);
                }
            }
        }
        p_z
    });
    end_timer!(timer);

    let mut lookup_z_commitments = vec![];

    let timer = start_timer!(|| "lookup intt and z msm");
    let mut tmp_buf = device.alloc_device_buffer::<C::ScalarExt>(size)?;
    let mut ntt_buf = device.alloc_device_buffer::<C::ScalarExt>(size)?;
    for (permuted_input, permuted_table, z) in lookups.iter_mut() {
        device.copy_from_host_to_device(&ntt_buf, &z[..])?;
        let commitment = msm_with_groups(&device, &g_lagrange_buf, &ntt_buf, size, 1)?;
        lookup_z_commitments.push(commitment);
        intt_raw(
            &device,
            &mut ntt_buf,
            &mut tmp_buf,
            &intt_pq_buf,
            &intt_omegas_buf,
            &divisor_buf,
            k,
        )?;
        device.copy_from_device_to_host(&mut z[..], &ntt_buf)?;

        device.copy_from_host_to_device(&ntt_buf, &permuted_input[..])?;
        intt_raw(
            &device,
            &mut ntt_buf,
            &mut tmp_buf,
            &intt_pq_buf,
            &intt_omegas_buf,
            &divisor_buf,
            k,
        )?;
        device.copy_from_device_to_host(&mut permuted_input[..], &ntt_buf)?;

        device.copy_from_host_to_device(&ntt_buf, &permuted_table[..])?;
        intt_raw(
            &device,
            &mut ntt_buf,
            &mut tmp_buf,
            &intt_pq_buf,
            &intt_omegas_buf,
            &divisor_buf,
            k,
        )?;
        device.copy_from_device_to_host(&mut permuted_table[..], &ntt_buf)?;
    }
    end_timer!(timer);

    let timer = start_timer!(|| "wait permutation_products");
    let mut permutation_products = permutation_products_handler.join().unwrap();
    end_timer!(timer);

    let timer = start_timer!(|| "permutation z msm and intt");
    for (i, z) in permutation_products.iter_mut().enumerate() {
        device.copy_from_host_to_device(&ntt_buf, &z[..])?;
        let commitment = msm_with_groups(&device, &g_lagrange_buf, &ntt_buf, size, 1)?;
        transcript.write_point(commitment).unwrap();
        intt_raw(
            &device,
            &mut ntt_buf,
            &mut tmp_buf,
            &intt_pq_buf,
            &intt_omegas_buf,
            &divisor_buf,
            k,
        )?;
        device.copy_from_device_to_host(&mut z[..], &ntt_buf)?;
    }

    for (i, commitment) in lookup_z_commitments.into_iter().enumerate() {
        transcript.write_point(commitment).unwrap();
    }

    end_timer!(timer);
    let vanishing =
        halo2_proofs::plonk::vanishing::Argument::commit(params, &pk.vk.domain, OsRng, transcript)
            .unwrap();

    let y: C::ScalarExt = *transcript.squeeze_challenge_scalar::<()>();
    println!("y is {:?}", y);

    let (ntt_omegas_buf, ntt_pq_buf) = ntt_prepare(&device, pk.get_vk().domain.get_omega(), k)?;

    let timer = start_timer!(|| "h_poly");
    {
        let timer = start_timer!(|| "advices intt");

        let mut check_buf = advices[0].clone();

        for advices in unsafe { Arc::get_mut_unchecked(&mut advices) }.iter_mut() {
            device.copy_from_host_to_device(&ntt_buf, &advices[..])?;
            intt_raw(
                &device,
                &mut ntt_buf,
                &mut tmp_buf,
                &intt_pq_buf,
                &intt_omegas_buf,
                &divisor_buf,
                k,
            )?;
            device.copy_from_device_to_host(&mut advices[..], &ntt_buf)?;
        }
        end_timer!(timer);
    }

    let fixed_ref = &pk.fixed_polys.iter().map(|x| &x[..]).collect::<Vec<_>>()[..];
    let advice_ref = &advices.iter().map(|x| &x[..]).collect::<Vec<_>>()[..];
    let instance_ref = &instance[0]
        .instance_polys
        .iter()
        .map(|x| &x[..])
        .collect::<Vec<_>>()[..];

    let h_poly = evaluate_h_gates(
        &device,
        &pk,
        fixed_ref,
        advice_ref,
        instance_ref,
        &permutation_products
            .iter()
            .map(|x| &x[..])
            .collect::<Vec<_>>()[..],
        y,
        beta,
        gamma,
        theta,
    )?;
    end_timer!(timer);

    Ok(())
}

struct EvalHContext<F: FieldExt> {
    y: Vec<F>,
    allocator: Vec<CudaDeviceBufRaw>,
    extended_allocator: Vec<CudaDeviceBufRaw>,
    extended_k: usize,
    size: usize,
    extended_size: usize,
    extended_ntt_omegas_buf: CudaDeviceBufRaw,
    extended_ntt_pq_buf: CudaDeviceBufRaw,
    coset_powers_buf: CudaDeviceBufRaw,
}

fn evaluate_h_gates<C: CurveAffine>(
    device: &CudaDevice,
    pk: &ProvingKey<C>,
    fixed: &[&[C::ScalarExt]],
    advice: &[&[C::ScalarExt]],
    instance: &[&[C::ScalarExt]],
    permutation_products: &[&[C::ScalarExt]],
    y: C::ScalarExt,
    beta: C::ScalarExt,
    gamma: C::ScalarExt,
    theta: C::ScalarExt,
) -> DeviceResult<Vec<C::ScalarExt, HugePageAllocator>> {
    let k = pk.get_vk().domain.k() as usize;
    let size = 1 << pk.get_vk().domain.k();
    let extended_k = pk.get_vk().domain.extended_k() as usize;
    let extended_size = 1 << extended_k;
    let extended_omega = pk.vk.domain.get_extended_omega();

    let (extended_ntt_omegas_buf, extended_ntt_pq_buf) =
        ntt_prepare(device, extended_omega, extended_k)?;
    let coset_powers_buf = device.alloc_device_buffer_from_slice(&[
        pk.get_vk().domain.g_coset,
        pk.get_vk().domain.g_coset_inv,
    ])?;
    let mut ctx = EvalHContext {
        y: vec![C::ScalarExt::one(), y],
        allocator: vec![],
        extended_allocator: vec![],
        extended_k,
        size,
        extended_size,
        extended_ntt_omegas_buf,
        extended_ntt_pq_buf,
        coset_powers_buf,
    };

    let mut res = Vec::new_in(HugePageAllocator);
    res.resize(extended_size, C::ScalarExt::zero());

    device.print_memory_info()?;
    /*
    for (i, v) in fixed.iter().enumerate() {
        println!("prover fixed {} [0..4] is {:?}", i, &v[0..4]);
    }
    for (i, v) in advice.iter().enumerate() {
        println!("prover advice {} [0..4] is {:?}", i, &v[0..4]);
    }
    for (i, v) in instance.iter().enumerate() {
        println!("prover instance {} [0..4] is {:?}", i, &v[0..4]);
    }
    */

    let timer = start_timer!(|| "prepare buffer");
    let fixed_buf = fixed
        .iter()
        .map(|x| device.alloc_device_buffer_from_slice(x))
        .collect::<Result<Vec<_>, _>>()?;
    let advice_buf = advice
        .iter()
        .map(|x| device.alloc_device_buffer_from_slice(x))
        .collect::<Result<Vec<_>, _>>()?;
    let instance_buf = instance
        .iter()
        .map(|x| device.alloc_device_buffer_from_slice(x))
        .collect::<Result<Vec<_>, _>>()?;
    end_timer!(timer);

    device.print_memory_info()?;
    let timer = start_timer!(|| "evaluate_h gates");
    let buf = evaluate_prove_expr(
        device,
        &pk.ev.gpu_gates_expr[0],
        &fixed_buf[..],
        &advice_buf[..],
        &instance_buf[..],
        &mut ctx,
    )?;
    let h_buf = match buf {
        EvalResult::SumBorrow(_, _, _) => unreachable!(),
        EvalResult::Single(_, buf) => buf,
    };
    device.print_memory_info()?;
    println!(
        "xixi {} {}",
        ctx.allocator.len(),
        ctx.extended_allocator.len()
    );
    device.copy_from_device_to_host(&mut res[..], &h_buf)?;
    println!("after gates res[0..4] is {:?}", &res[0..4]);
    end_timer!(timer);

    assert!(pk.ev.gpu_gates_expr.len() == 1);
    //analysis_v2(&pk.ev.gpu_gates_expr[0], 0);

    /*
       let y_buf = device.alloc_device_buffer_from_slice(&[y][..])?;
       let beta_buf = device.alloc_device_buffer_from_slice(&[beta][..])?;
       let gamma_buf = device.alloc_device_buffer_from_slice(&[gamma][..])?;
       let theta_buf = device.alloc_device_buffer_from_slice(&[theta][..])?;

       let timer = start_timer!(|| "evaluate_h permutation");
       if permutation_products.len() > 0 {
           let blinding_factors = pk.vk.cs.blinding_factors();
           let last_rotation = (ctx.size - (blinding_factors + 1)) << (extended_k - k);
           let chunk_len = pk.vk.cs.degree() - 2;

           let l0 = &pk.l0;
           let l_last = &pk.l_last;
           let l_active_row = &pk.l_active_row;

           let l0_buf = do_extended_fft_v2(device, &mut ctx, &l0.values[..])?;
           let l_last_buf = do_extended_fft_v2(device, &mut ctx, &l_last.values[..])?;
           let l_active_buf = device.alloc_device_buffer_from_slice(&l_active_row.values[..])?;

           let extended_p_buf = permutation_products
               .iter()
               .map(|x| do_extended_fft_v2(device, &mut ctx, x))
               .collect::<Result<Vec<_>, _>>()?;

           {
               permutation_eval_h_p1(
                   device,
                   &h_buf,
                   extended_p_buf.first().unwrap(),
                   extended_p_buf.last().unwrap(),
                   &l0_buf,
                   &l_last_buf,
                   &y_buf,
                   ctx.extended_size,
               )?;

               permutation_eval_h_p2(
                   device,
                   &h_buf,
                   &extended_p_buf[..],
                   &l0_buf,
                   &l_last_buf,
                   &y_buf,
                   last_rotation,
                   ctx.extended_size,
               )?;

               let mut curr_delta = beta * &C::Scalar::ZETA;
               for ((extended_p_buf, columns), polys) in extended_p_buf
                   .iter()
                   .zip(pk.vk.cs.permutation.columns.chunks(chunk_len))
                   .zip(pk.permutation.polys.chunks(chunk_len))
               {
                   let buf = ctx.extended_allocator.pop();
                   let l = if buf.is_none() {
                       device.alloc_device_buffer::<C::ScalarExt>(ctx.extended_size)?
                   } else {
                       buf.unwrap()
                   };
                   buffer_copy_with_shift::<C::ScalarExt>(
                       &device,
                       &l,
                       extended_p_buf,
                       1 << (extended_k - k),
                       ctx.extended_size,
                   )?;

                   let buf = ctx.extended_allocator.pop();
                   let r = if buf.is_none() {
                       device.alloc_device_buffer::<C::ScalarExt>(ctx.extended_size)?
                   } else {
                       buf.unwrap()
                   };
                   buffer_copy_with_shift::<C::ScalarExt>(
                       &device,
                       &l,
                       extended_p_buf,
                       0,
                       ctx.extended_size,
                   )?;

                   for (value_buf, permutation) in columns
                       .iter()
                       .map(|&column| match column.column_type() {
                           Any::Advice => &advice_buf[column.index()],
                           Any::Fixed => &fixed_buf[column.index()],
                           Any::Instance => &instance_buf[column.index()],
                       })
                       .zip(polys.iter())
                   {
                       let buf = ctx.extended_allocator.pop();
                       let mut tmp = if buf.is_none() {
                           device.alloc_device_buffer::<C::ScalarExt>(ctx.extended_size)?
                       } else {
                           buf.unwrap()
                       };
                       let buf = ctx.allocator.pop();
                       let p_coset_buf = if buf.is_none() {
                           device.alloc_device_buffer::<C::ScalarExt>(ctx.extended_size)?
                       } else {
                           buf.unwrap()
                       };
                       device.copy_from_host_to_device(&p_coset_buf, &permutation.values[..])?;

                       permutation_eval_h_l(
                           &device,
                           &tmp,
                           &beta_buf,
                           &gamma_buf,
                           &p_coset_buf,
                           ctx.size,
                       )?;

                       do_extended_fft(&device, &mut ctx, &mut tmp)?;

                       field_op_v2::<C::ScalarExt>(
                           &device,
                           &l,
                           Some(&l),
                           None,
                           Some(&tmp),
                           None,
                           ctx.extended_size,
                           FieldOp::Mul,
                       )?;

                       let curr_delta_buf =
                           device.alloc_device_buffer_from_slice(&[curr_delta][..])?;

                       permutation_eval_h_r(&device, &tmp, &curr_delta_buf, &gamma_buf, &value_buf)?;
                       do_extended_fft(&device, &mut ctx, &mut tmp)?;
                       field_op_v2::<C::ScalarExt>(
                           &device,
                           &r,
                           Some(&r),
                           None,
                           Some(&tmp),
                           None,
                           ctx.extended_size,
                           FieldOp::Mul,
                       )?;
                       curr_delta *= &C::Scalar::DELTA;
                       field_op_v2::<C::ScalarExt>(
                           &device,
                           &l,
                           Some(&l),
                           None,
                           Some(&r),
                           None,
                           ctx.extended_size,
                           FieldOp::Sub,
                       )?;
                       field_op_v2::<C::ScalarExt>(
                           &device,
                           &l,
                           Some(&l),
                           None,
                           Some(&l_active_buf),
                           None,
                           ctx.extended_size,
                           FieldOp::Mul,
                       )?;
                       field_op_v2::<C::ScalarExt>(
                           &device,
                           &h_buf,
                           Some(&h_buf),
                           None,
                           None,
                           Some(y),
                           ctx.extended_size,
                           FieldOp::Mul,
                       )?;
                       field_op_v2::<C::ScalarExt>(
                           &device,
                           &h_buf,
                           Some(&h_buf),
                           None,
                           Some(&l),
                           None,
                           ctx.extended_size,
                           FieldOp::Sum,
                       )?;
                   }

                   ctx.extended_allocator.push(l);
                   ctx.extended_allocator.push(r);
               }
           }
       }
       end_timer!(timer);
    */
    Ok(res)
}

fn do_extended_fft_v2<F: FieldExt>(
    device: &CudaDevice,
    ctx: &mut EvalHContext<F>,
    data: &[F],
) -> DeviceResult<CudaDeviceBufRaw> {
    let buf = ctx.extended_allocator.pop();
    let mut buf = if buf.is_none() {
        device.alloc_device_buffer::<F>(ctx.extended_size)?
    } else {
        buf.unwrap()
    };
    device.copy_from_host_to_device::<F>(&buf, data)?;
    do_extended_fft(device, ctx, &mut buf)?;

    Ok(buf)
}

fn do_extended_fft<F: FieldExt>(
    device: &CudaDevice,
    ctx: &mut EvalHContext<F>,
    data: &mut CudaDeviceBufRaw,
) -> DeviceResult<()> {
    //let timer = start_timer!(|| "handle extended fft");
    //let timer1 = start_timer!(|| "alloc buffer");
    let tmp = ctx.extended_allocator.pop();
    let mut tmp = if tmp.is_none() {
        println!("alloc");
        device.alloc_device_buffer::<F>(ctx.extended_size)?
    } else {
        tmp.unwrap()
    };
    //end_timer!(timer1);
    //let timer1 = start_timer!(|| "prepare");
    extended_prepare(
        device,
        data,
        &ctx.coset_powers_buf,
        3,
        ctx.size,
        ctx.extended_size,
    )?;
    device.synchronize()?;
    //end_timer!(timer1);
    //let timer1 = start_timer!(|| "ntt");
    ntt_raw(
        device,
        data,
        &mut tmp,
        &ctx.extended_ntt_pq_buf,
        &ctx.extended_ntt_omegas_buf,
        ctx.extended_k,
    )?;
    device.synchronize()?;
    // end_timer!(timer1);
    ctx.extended_allocator.push(tmp);
    // end_timer!(timer);
    Ok(())
}

enum EvalResult<'a, F: FieldExt> {
    SumBorrow(
        usize,
        Vec<(&'a CudaDeviceBufRaw, isize, Option<F>)>,
        Option<F>,
    ),
    Single(usize, CudaDeviceBufRaw),
}

impl<'a, F: FieldExt> EvalResult<'a, F> {
    fn deg(&self) -> &usize {
        match self {
            EvalResult::SumBorrow(deg, _, _) => deg,
            EvalResult::Single(deg, _) => deg,
        }
    }

    fn eval(
        self,
        device: &CudaDevice,
        target_deg: usize,
        ctx: &mut EvalHContext<F>,
    ) -> DeviceResult<CudaDeviceBufRaw> {
        let (mut buf, deg) = match self {
            EvalResult::SumBorrow(deg, arr, c) => {
                assert!(deg == 1);
                let buf = ctx.extended_allocator.pop();
                let res = if buf.is_none() {
                    device.alloc_device_buffer::<F>(ctx.extended_size)?
                } else {
                    buf.unwrap()
                };
                field_mul_sum_vec(device, &res, &arr, ctx.size)?;
                if c.is_some() {
                    assert!(deg == 1);
                    let mut v = [F::zero()];
                    device.copy_from_device_to_host(&mut v[..], &res)?;
                    v[0] += c.unwrap();
                    device.copy_from_host_to_device(&res, &v[..])?;
                }
                (res, deg)
            }
            EvalResult::Single(deg, buf) => (buf, deg),
        };

        // switch to lagrange coeff
        if deg != target_deg {
            assert!(target_deg == 4);
            do_extended_fft(device, ctx, &mut buf)?;
        }
        Ok(buf)
    }

    fn is_borrow(&self) -> bool {
        match self {
            EvalResult::SumBorrow(_, _, _) => true,
            EvalResult::Single(_, _) => false,
        }
    }

    fn is_const(&self) -> Option<F> {
        match self {
            EvalResult::SumBorrow(_, arr, c) => {
                if arr.is_empty() {
                    c.clone()
                } else {
                    None
                }
            }
            EvalResult::Single(_, _) => None,
        }
    }

    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (
                EvalResult::SumBorrow(deg, mut l, mut l_c),
                EvalResult::SumBorrow(r_deg, mut r, r_c),
            ) => {
                assert!(deg == r_deg);
                l.append(&mut r);
                if r_c.is_some() {
                    if l_c.is_some() {
                        l_c = Some(l_c.unwrap() + r_c.unwrap());
                    } else {
                        l_c = r_c;
                    }
                }
                EvalResult::SumBorrow(deg, l, l_c)
            }
            _ => unreachable!(),
        }
    }

    fn scale(&mut self, device: &CudaDevice, v: F, ctx: &mut EvalHContext<F>) -> DeviceResult<()> {
        match self {
            EvalResult::SumBorrow(_, arr, c) => {
                *c = c.map(|x| v * x);
                for (_, _, c) in arr {
                    *c = if c.is_some() {
                        Some(c.unwrap() * v)
                    } else {
                        Some(v)
                    }
                }
            }
            EvalResult::Single(deg, buf) => {
                field_op_v2(
                    device,
                    &buf,
                    Some(&buf),
                    None,
                    None,
                    Some(v),
                    ctx.size * *deg,
                    FieldOp::Mul,
                )?;
            }
        }
        Ok(())
    }
}

fn eval_ys<F: FieldExt>(ys: &std::collections::BTreeMap<u32, F>, ctx: &mut EvalHContext<F>) -> F {
    let max_y_order = *ys.keys().max().unwrap();
    for _ in (ctx.y.len() as u32)..=max_y_order {
        ctx.y.push(ctx.y[1] * ctx.y.last().unwrap());
    }
    ys.iter().fold(F::zero(), |acc, (y_order, f)| {
        acc + ctx.y[*y_order as usize] * f
    })
}

fn evaluate_prove_expr<'a, F: FieldExt>(
    device: &CudaDevice,
    expr: &ProveExpression<F>,
    fixed_buf: &'a [CudaDeviceBufRaw],
    advice_buf: &'a [CudaDeviceBufRaw],
    instance_buf: &'a [CudaDeviceBufRaw],
    ctx: &mut EvalHContext<F>,
) -> DeviceResult<EvalResult<'a, F>> {
    match expr {
        ProveExpression::Unit(u) => {
            //let timer = start_timer!(|| "handle unit");
            let (src, rotation) = match u {
                ProveExpressionUnit::Fixed {
                    column_index,
                    rotation,
                } => (&fixed_buf[*column_index], rotation),
                ProveExpressionUnit::Advice {
                    column_index,
                    rotation,
                } => (&advice_buf[*column_index], rotation),
                ProveExpressionUnit::Instance {
                    column_index,
                    rotation,
                } => (&instance_buf[*column_index], rotation),
            };

            let rot = rotation.0 as isize;

            Ok(EvalResult::SumBorrow(1, vec![(src, rot, None)], None))
        }
        ProveExpression::Op(l, r, op) => {
            let l = evaluate_prove_expr(device, l, fixed_buf, advice_buf, instance_buf, ctx)?;
            let r = evaluate_prove_expr(device, r, fixed_buf, advice_buf, instance_buf, ctx)?;

            let l_deg = *l.deg();
            let r_deg = *r.deg();

            match op {
                Bop::Sum => {
                    if l_deg == r_deg && l.is_borrow() && r.is_borrow() {
                        assert!(l_deg == 1);
                        return Ok(l.merge(r));
                    }
                    if true || l_deg.max(r_deg) == 4 {
                        let l = l.eval(device, 4, ctx)?;
                        let r = r.eval(device, 4, ctx)?;
                        field_sum::<F>(device, &l, &r, ctx.extended_size)?;
                        ctx.extended_allocator.push(r);
                        return Ok(EvalResult::Single(4, l));
                    }
                    unreachable!()
                    /* else {
                        let l = l.eval(device, l_deg, ctx)?;
                        let r = r.eval(device, r_deg, ctx)?;
                        let (res, other) = if l_deg >= r_deg { (l, r) } else { (r, l) };
                        field_sum::<F>(device, &res, &other, ctx.size)?;
                        ctx.extended_allocator.push(other);
                        Ok(EvalResult::Single(l_deg.max(r_deg), res))
                    } */
                }
                Bop::Product => {
                    let l = l.eval(device, 4, ctx)?;
                    let r = r.eval(device, 4, ctx)?;
                    field_mul::<F>(device, &l, &r, ctx.extended_size)?;
                    ctx.extended_allocator.push(r);
                    Ok(EvalResult::Single(4, l))
                }
            }
        }
        ProveExpression::Y(ys) => {
            let c = eval_ys(ys, ctx);
            Ok(EvalResult::SumBorrow(1, vec![], Some(c)))
        }
        ProveExpression::Scale(l, ys) => {
            let mut l = evaluate_prove_expr(device, l, fixed_buf, advice_buf, instance_buf, ctx)?;
            let c = eval_ys(ys, ctx);
            l.scale(device, c, ctx)?;
            Ok(l)
        }
    }
}

fn analysis<F: FieldExt>(expr: &ProveExpression<F>) -> usize {
    match expr {
        ProveExpression::Unit(u) => {
            let rotation = match u {
                ProveExpressionUnit::Fixed { rotation, .. } => rotation,
                ProveExpressionUnit::Advice { rotation, .. } => rotation,
                ProveExpressionUnit::Instance { rotation, .. } => rotation,
            };
            println!("handle unit {:?}", rotation);
            return 1;
        }
        ProveExpression::Op(l, r, op) => {
            let l_dep = analysis(l);
            let r_dep = analysis(r);
            match op {
                Bop::Sum => {
                    if l_dep != r_dep {
                        println!(
                            "handle deep upgrade {} {}",
                            l_dep.min(r_dep),
                            l_dep.max(r_dep)
                        );
                    }

                    println!("handle sum {} {}", l_dep, r_dep);
                    return l_dep.max(r_dep);
                }
                Bop::Product => {
                    if l_dep == 1 && r_dep == 1 {
                        println!("handle deep upgrade {} {}", 1, 2);
                        println!("handle deep upgrade {} {}", 1, 2);
                        println!("handle mul {}", 2);
                        return 2;
                    } else {
                        println!("handle mul {}", l_dep.max(r_dep));
                        if l_dep < 4 {
                            println!("handle deep upgrade 4 from {}", l_dep);
                        }
                        if r_dep < 4 {
                            println!("handle deep upgrade 4 from {}", r_dep);
                        }
                        return 4;
                    }
                }
            }
        }
        ProveExpression::Y(_) => {
            println!("handle y");
            return 1;
        }
        ProveExpression::Scale(l, _) => {
            let l_dep = analysis(l);
            println!("handle scale {}", l_dep);
            return l_dep;
        }
    }
}

fn print_ident(ident: usize) {
    for i in 0..ident {
        print!("-");
    }
}

fn analysis_v2<F: FieldExt>(expr: &ProveExpression<F>, ident: usize) -> usize {
    match expr {
        ProveExpression::Unit(u) => {
            let rotation = match u {
                ProveExpressionUnit::Fixed { rotation, .. } => rotation,
                ProveExpressionUnit::Advice { rotation, .. } => rotation,
                ProveExpressionUnit::Instance { rotation, .. } => rotation,
            };
            print_ident(ident);
            println!("handle unit {:?}", rotation);
            return 1;
        }
        ProveExpression::Op(l, r, op) => {
            let l_dep = analysis_v2(l, ident + 2);
            let r_dep = analysis_v2(r, ident + 2);

            match op {
                Bop::Sum => {
                    if l_dep != r_dep {
                        /*
                        print_ident(ident);
                        println!(
                            "handle deep upgrade {} {}",
                            l_dep.min(r_dep),
                            l_dep.max(r_dep)
                        ); */
                    }
                    print_ident(ident);
                    println!("handle sum {} {}", l_dep, r_dep);
                    return l_dep.max(r_dep);
                }
                Bop::Product => {
                    if l_dep == 1 && r_dep == 1 {
                        /*
                        print_ident(ident);
                        println!("handle deep upgrade {} {}", 1, 2);
                        print_ident(ident);
                        println!("handle deep upgrade {} {}", 1, 2); */
                        print_ident(ident);
                        println!("handle mul {}", 2);
                        return 2;
                    } else {
                        print_ident(ident);
                        println!("handle mul {}", l_dep.max(r_dep));
                        /*
                        if l_dep < 4 {
                            print_ident(ident);
                            println!("handle deep upgrade 4 from {}", l_dep);
                        }
                        if r_dep < 4 {
                            print_ident(ident);
                            println!("handle deep upgrade 4 from {}", r_dep);
                        }*/
                        return 4;
                    }
                }
            }
        }
        ProveExpression::Y(_) => {
            print_ident(ident);
            println!("handle y");
            return 1;
        }
        ProveExpression::Scale(l, _) => {
            let l_dep = analysis_v2(l, ident + 2);
            print_ident(ident);
            println!("handle scale {}", l_dep);
            return l_dep;
        }
    }
}
