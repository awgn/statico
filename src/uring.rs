//! Utility module to configure io_uring parameters consistently across all supported runtimes.

use crate::options::Options;

#[allow(dead_code)]
pub trait UringConfigurator {
    fn configure_uring(&mut self, opts: &Options);
}

#[cfg(all(target_os = "linux", any(feature = "monoio", feature = "tokio_uring")))]
impl UringConfigurator for io_uring::Builder {
    fn configure_uring(&mut self, opts: &Options) {
        let num_entries = opts.uring_entries.next_power_of_two();
        let cqsize = num_entries * 2;

        self.setup_single_issuer().setup_cqsize(cqsize);

        if let Some(idle) = opts.uring_sqpoll {
            self.setup_sqpoll(idle);
        } else {
            self.setup_coop_taskrun().setup_taskrun_flag();
        }
    }
}

#[cfg(feature = "compio")]
impl UringConfigurator for compio::driver::ProactorBuilder {
    fn configure_uring(&mut self, opts: &Options) {
        #[cfg(target_os = "linux")]
        {
            let num_entries = opts.uring_entries.next_power_of_two();
            self.capacity(num_entries);

            if let Some(idle) = opts.uring_sqpoll {
                self.sqpoll_idle(std::time::Duration::from_millis(idle as u64));
            } else {
                self.coop_taskrun(true).taskrun_flag(true);
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = opts;
            // Default capacity when not on Linux since uring_entries is not available
            self.capacity(4096);
        }
    }
}
