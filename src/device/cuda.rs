use core::cell::RefCell;
use core::mem;
use std::collections::HashMap;
use std::mem::size_of;
use std::{ffi::c_void, sync::Mutex};

use cuda_runtime_sys::{cudaError, cudaStream_t};

use super::{Device, DeviceBuf, Error};
use crate::device::DeviceResult;

thread_local! {
    static ACITVE_CUDA_DEVICE: RefCell<i32> = RefCell::new(-1);
}

const HUGE_BUFFER_SIZE: usize = 1 << 28;

lazy_static! {
    pub static ref CUDA_BUFFER_CACHE: Mutex<HashMap::<(i32, usize), Vec<usize>>> =
        Mutex::new(HashMap::new());
}

#[derive(Debug, Clone)]
pub struct CudaDevice {
    device: i32,
}

impl Drop for CudaDevice {
    fn drop(&mut self) {
        /*
        ACITVE_CUDA_DEVICE.with(|x| {
            if *x.borrow() != self.device {
                *x.borrow_mut() = -1;
            }
        });
        */
    }
}

impl CudaDevice {
    pub(crate) fn acitve_ctx(&self) -> DeviceResult<()> {
        ACITVE_CUDA_DEVICE.with(|x| {
            if *x.borrow() != self.device {
                *x.borrow_mut() = self.device
            }
        });

        unsafe {
            let res = cuda_runtime_sys::cudaSetDevice(self.device);
            to_result((), res, "fail to set device")
        }
    }
}

#[inline]
pub(crate) fn to_result<T>(value: T, res: cudaError, msg: &'static str) -> DeviceResult<T> {
    if res != cudaError::cudaSuccess {
        Err(Error::DeviceError(format!(
            "Cuda Error({:?}): {}",
            res, msg
        )))
    } else {
        Ok(value)
    }
}

pub trait CudaBuffer {
    fn ptr(&self) -> *mut c_void;
    fn device<'a>(&'a self) -> &'a CudaDevice;
}

impl CudaBuffer for CudaDeviceBufRaw {
    fn ptr(&self) -> *mut c_void {
        self.ptr
    }

    fn device<'a>(&'a self) -> &'a CudaDevice {
        &self.device
    }
}

#[derive(Debug)]
pub struct CudaDeviceBufRaw {
    pub(crate) ptr: *mut c_void,
    pub(crate) device: CudaDevice,
    pub(crate) size: usize,
}

extern "C" {
    pub fn cudaFreeAsync(ptr: *mut c_void, stream: cudaStream_t) -> cudaError;
}

impl Drop for CudaDeviceBufRaw {
    fn drop(&mut self) {
        if self.size < HUGE_BUFFER_SIZE {
            let mut cache = CUDA_BUFFER_CACHE.lock().unwrap();
            let arr = cache
                .entry((self.device.device, self.size))
                .or_insert(vec![]);
            assert!(!arr.contains(&(self.ptr() as usize)));
            arr.push(self.ptr() as usize);
        } else {
            self.device().acitve_ctx().unwrap();
            unsafe {
                //let timer = start_timer!(|| "cuda free");
                let res = cudaFreeAsync(self.ptr(), 0usize as _);
                to_result((), res, "fail to free device memory").unwrap();
                //end_timer!(timer);
            }
        }
    }
}

impl DeviceBuf for CudaDeviceBufRaw {}

impl CudaDevice {
    pub fn copy_from_host_to_device_async<T>(
        &self,
        dst: &CudaDeviceBufRaw,
        src: &[T],
        stream: cudaStream_t,
    ) -> DeviceResult<()> {
        self.acitve_ctx()?;
        unsafe {
            let res = cuda_runtime_sys::cudaMemcpyAsync(
                dst.ptr(),
                src.as_ptr() as _,
                src.len() * mem::size_of::<T>(),
                cuda_runtime_sys::cudaMemcpyKind::cudaMemcpyHostToDevice,
                stream,
            );
            to_result((), res, "fail to copy memory from host to device")
        }
    }

    pub fn copy_from_device_to_host_async<T>(
        &self,
        dst: &mut [T],
        src: &CudaDeviceBufRaw,
        stream: cudaStream_t,
    ) -> DeviceResult<()> {
        self.acitve_ctx()?;
        unsafe {
            let res = cuda_runtime_sys::cudaMemcpyAsync(
                dst.as_ptr() as _,
                src.ptr(),
                dst.len() * mem::size_of::<T>(),
                cuda_runtime_sys::cudaMemcpyKind::cudaMemcpyDeviceToHost,
                stream,
            );
            to_result((), res, "fail to copy memory from device to host")
        }
    }

    pub fn copy_from_device_to_host_async_v2<T>(
        &self,
        dst: &mut [T],
        src: &CudaDeviceBufRaw,
        offset: isize,
        stream: Option<cudaStream_t>,
    ) -> DeviceResult<()> {
        self.acitve_ctx()?;
        unsafe {
            let res = cuda_runtime_sys::cudaMemcpyAsync(
                dst.as_ptr() as _,
                src.ptr().offset(offset * mem::size_of::<T>() as isize),
                dst.len() * mem::size_of::<T>(),
                cuda_runtime_sys::cudaMemcpyKind::cudaMemcpyDeviceToHost,
                stream.unwrap_or(0usize as _),
            );
            to_result((), res, "fail to copy memory from device to host")
        }
    }

    fn _alloc_device_buffer<T>(&self, size: usize, zero: bool) -> DeviceResult<CudaDeviceBufRaw> {
        //println!("alloc device memory {}", size * mem::size_of::<T>());
        //self.print_memory_info()?;
        unsafe {
            let size = size * mem::size_of::<T>();
            {
                let mut cache = CUDA_BUFFER_CACHE.lock().unwrap();
                let arr = cache.entry((self.device, size)).or_insert(vec![]);

                if arr.len() > 0 {
                    let ret = CudaDeviceBufRaw {
                        ptr: arr.pop().unwrap() as *mut c_void,
                        device: self.clone(),
                        size,
                    };
                    if zero {
                        cuda_runtime_sys::cudaMemset(ret.ptr(), 0, size);
                    }
                    return Ok(ret);
                }
            }

            self.acitve_ctx()?;
            let mut ptr = 0 as *mut c_void;
            let res = cuda_runtime_sys::cudaMalloc(&mut ptr, size);
            //self.print_memory_info()?;
            to_result(
                CudaDeviceBufRaw {
                    ptr,
                    device: self.clone(),
                    size,
                },
                res,
                "fail to alloc device memory",
            )
        }
    }

    pub fn copy_from_device_to_device_async<T>(
        &self,
        dst: &CudaDeviceBufRaw,
        src: &CudaDeviceBufRaw,
        size: usize,
        stream: cudaStream_t,
    ) -> DeviceResult<()> {
        self.acitve_ctx()?;
        unsafe {
            let res = cuda_runtime_sys::cudaMemcpyAsync(
                dst.ptr(),
                src.ptr(),
                size * mem::size_of::<T>(),
                cuda_runtime_sys::cudaMemcpyKind::cudaMemcpyDeviceToDevice,
                stream,
            );
            to_result((), res, "fail to copy memory from device to host")
        }
    }
}

impl Device<CudaDeviceBufRaw> for CudaDevice {
    fn get_device_count() -> DeviceResult<usize> {
        let mut count = 0;
        unsafe {
            let res = cuda_runtime_sys::cudaGetDeviceCount(&mut count);
            to_result(count as usize, res, "fail to get device count")
        }
    }

    fn get_device(idx: usize) -> DeviceResult<Self> {
        let count = Self::get_device_count()?;
        if idx < count {
            Ok(Self { device: idx as i32 })
        } else {
            Err(Error::DeviceError(format!(
                "Cuda Error(): Invalid device idx {}",
                idx
            )))
        }
    }

    fn print_memory_info(&self) -> DeviceResult<()> {
        self.acitve_ctx()?;
        unsafe {
            let mut free = 0;
            let mut total = 0;
            cuda_runtime_sys::cudaMemGetInfo(&mut free, &mut total);
            println!("free is {},total is {}", free, total);
        }
        Ok(())
    }

    fn alloc_device_buffer<T>(&self, size: usize) -> DeviceResult<CudaDeviceBufRaw> {
        self._alloc_device_buffer::<T>(size, true)
    }

    fn alloc_device_buffer_from_slice<T>(&self, data: &[T]) -> DeviceResult<CudaDeviceBufRaw> {
        let buf = self._alloc_device_buffer::<T>(data.len(), false)?;
        self.copy_from_host_to_device(&buf, data)?;
        Ok(buf)
    }

    fn copy_from_host_to_device<T>(&self, dst: &CudaDeviceBufRaw, src: &[T]) -> DeviceResult<()> {
        self.acitve_ctx()?;
        unsafe {
            let res = cuda_runtime_sys::cudaMemcpyAsync(
                dst.ptr(),
                src.as_ptr() as _,
                src.len() * mem::size_of::<T>(),
                cuda_runtime_sys::cudaMemcpyKind::cudaMemcpyHostToDevice,
                0usize as *mut _,
            );
            to_result((), res, "fail to copy memory from host to device")
        }
    }

    fn copy_from_device_to_host<T>(
        &self,
        dst: &mut [T],
        src: &CudaDeviceBufRaw,
    ) -> DeviceResult<()> {
        self.acitve_ctx()?;
        unsafe {
            let res = cuda_runtime_sys::cudaMemcpy(
                dst.as_ptr() as _,
                src.ptr(),
                dst.len() * mem::size_of::<T>(),
                cuda_runtime_sys::cudaMemcpyKind::cudaMemcpyDeviceToHost,
            );
            to_result((), res, "fail to copy memory from device to host")
        }
    }

    fn copy_from_device_to_device<T>(
        &self,
        dst: &CudaDeviceBufRaw,
        dst_offset: usize,
        src: &CudaDeviceBufRaw,
        src_offset: usize,
        len: usize,
    ) -> DeviceResult<()> {
        self.acitve_ctx()?;
        unsafe {
            let res = cuda_runtime_sys::cudaMemcpy(
                (dst.ptr()).offset((dst_offset * mem::size_of::<T>()) as isize),
                (src.ptr()).offset((src_offset * mem::size_of::<T>()) as isize),
                len * mem::size_of::<T>(),
                cuda_runtime_sys::cudaMemcpyKind::cudaMemcpyDeviceToDevice,
            );
            to_result((), res, "fail to copy memory from device to device")
        }
    }

    fn synchronize(&self) -> DeviceResult<()> {
        self.acitve_ctx()?;
        unsafe {
            let res = cuda_runtime_sys::cudaDeviceSynchronize();
            to_result((), res, "fail to synchronize")
        }
    }

    fn pin_memory<T>(&self, dst: &[T]) -> DeviceResult<()> {
        self.acitve_ctx()?;
        unsafe {
            let res: cudaError = cuda_runtime_sys::cudaHostRegister(
                dst.as_ptr() as *mut _,
                dst.len() * size_of::<T>(),
                cuda_runtime_sys::cudaHostAllocMapped,
            );
            if res == cudaError::cudaErrorHostMemoryAlreadyRegistered {
                return Ok(());
            }
            to_result((), res, "fail to synchronize")
        }
    }

    fn unpin_memory<T>(&self, dst: &[T]) -> DeviceResult<()> {
        self.acitve_ctx()?;
        unsafe {
            let res = cuda_runtime_sys::cudaHostUnregister(dst.as_ptr() as *mut _);
            to_result((), res, "fail to synchronize")
        }
    }
}
