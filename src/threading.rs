//! Thread priority and CPU affinity helpers shared by the pipeline and schedulers.
use std::error::Error;

/// Configure CPU affinity for the process when available.
pub fn set_cpu_affinity() -> Result<(), Box<dyn Error>> {
    #[cfg(target_os = "linux")]
    {
        use libc::{cpu_set_t, getpid, sched_setaffinity, CPU_SET, CPU_ZERO};

        unsafe {
            let mut set: cpu_set_t = std::mem::zeroed();
            CPU_ZERO(&mut set);
            CPU_SET(0, &mut set);
            CPU_SET(1, &mut set);
            CPU_SET(2, &mut set);

            let pid = getpid();
            let _ = sched_setaffinity(pid, std::mem::size_of::<cpu_set_t>(), &set);
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // CPU affinity not available on this platform
    }
    Ok(())
}

/// Attempt to set a cooperative thread priority on supported platforms.
pub fn set_thread_priority(priority: i32) {
    #[cfg(target_os = "linux")]
    {
        use libc::{
            pthread_self, pthread_setschedparam, sched_param, SCHED_FIFO, SCHED_OTHER, SCHED_RR,
        };
        use std::mem;

        let (policy, sched_priority) = if priority >= 3 {
            (SCHED_FIFO, 90)
        } else if priority == 2 {
            (SCHED_FIFO, 70)
        } else if priority == 1 {
            (SCHED_RR, 30)
        } else {
            (SCHED_OTHER, 0)
        };

        unsafe {
            let mut param: sched_param = mem::zeroed();
            param.sched_priority = sched_priority;
            let thread = pthread_self();
            let _ = pthread_setschedparam(thread, policy, &param);
        }
    }
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        // macOS uses Quality of Service (QoS) classes instead of numeric priorities
        // Map priority levels to QoS classes:
        // Priority 2 (high) -> QOS_CLASS_USER_INTERACTIVE or QOS_CLASS_USER_INITIATED
        // Priority 1 (low) -> QOS_CLASS_UTILITY or QOS_CLASS_BACKGROUND

        // Define QoS class constants (from pthread/qos.h)
        // QOS_CLASS_USER_INTERACTIVE = 0x21 (highest, for UI)
        const QOS_CLASS_USER_INITIATED: u32 = 0x19; // High priority for critical work
        const QOS_CLASS_UTILITY: u32 = 0x15; // Medium priority for utility work
        const QOS_CLASS_BACKGROUND: u32 = 0x09; // Low priority for background work

        // Select QoS class based on priority
        let qos_class = if priority >= 2 {
            QOS_CLASS_USER_INITIATED // High priority for EDF and critical threads
        } else if priority == 1 {
            QOS_CLASS_UTILITY // Lower priority for statistics thread
        } else {
            QOS_CLASS_BACKGROUND // Lowest priority
        };

        unsafe {
            // Set QoS class for current thread
            // pthread_set_qos_class_self_np signature: int pthread_set_qos_class_self_np(qos_class_t qos_class, int relative_priority);
            extern "C" {
                fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
            }

            let _ = pthread_set_qos_class_self_np(qos_class, 0);

            // Also set thread name if possible
            if let Ok(name) = CString::new(format!("Thread-Priority-{}", priority)) {
                let _ = libc::pthread_setname_np(name.as_ptr());
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        // Thread priority setting not implemented for this platform
        let _ = priority;
    }
}

/// Attempt to pin the current thread to a specific core when supported.
pub fn set_thread_core(core_id: usize) {
    #[cfg(target_os = "linux")]
    unsafe {
        use libc::{cpu_set_t, pthread_self, pthread_setaffinity_np, CPU_SET, CPU_ZERO};
        let mut set: cpu_set_t = std::mem::zeroed();
        CPU_ZERO(&mut set);
        CPU_SET(core_id, &mut set);
        let _ = pthread_setaffinity_np(pthread_self(), std::mem::size_of::<cpu_set_t>(), &set);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = core_id;
    }
}
