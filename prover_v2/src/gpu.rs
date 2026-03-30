use once_cell::sync::Lazy;
use parking_lot::{Condvar, Mutex};

struct GpuSemaphore {
    available: Mutex<usize>,
    cvar: Condvar,
    max: usize,
}

impl GpuSemaphore {
    fn new(max: usize) -> Self {
        Self {
            available: Mutex::new(max),
            cvar: Condvar::new(),
            max,
        }
    }

    fn acquire(&'static self) -> GpuPermit {
        let mut available = self.available.lock();
        while *available == 0 {
            self.cvar.wait(&mut available);
        }
        *available -= 1;
        GpuPermit { semaphore: self }
    }

    fn release(&self) {
        let mut available = self.available.lock();
        if *available < self.max {
            *available += 1;
        }
        self.cvar.notify_one();
    }
}

pub(crate) struct GpuPermit {
    semaphore: &'static GpuSemaphore,
}

impl Drop for GpuPermit {
    fn drop(&mut self) {
        self.semaphore.release();
    }
}

fn gpu_parallelism() -> usize {
    let value = std::env::var("PROVER_GPU_PARALLELISM")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1);
    std::cmp::max(1, value)
}

static GPU_SEMAPHORE: Lazy<GpuSemaphore> = Lazy::new(|| GpuSemaphore::new(gpu_parallelism()));

pub(crate) fn acquire_gpu_permit() -> GpuPermit {
    GPU_SEMAPHORE.acquire()
}
