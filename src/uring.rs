//! Utility module to configure io_uring parameters consistently across all supported runtimes.

/// Configures an io_uring builder (works for both monoio's and tokio_uring's builder).
#[macro_export]
macro_rules! configure_io_uring {
    ($opts:expr, $uring:expr) => {{
        let num_entries = $opts.uring_entries.next_power_of_two();
        let cqsize = num_entries * 2;

        $uring.setup_single_issuer().setup_cqsize(cqsize);

        if let Some(idle) = $opts.uring_sqpoll {
            $uring.setup_sqpoll(idle);
        } else {
            $uring.setup_coop_taskrun().setup_taskrun_flag();
        }
    }};
}

/// Configures compio's ProactorBuilder with optimal io_uring settings on Linux,
/// and fallback defaults on other platforms.
#[macro_export]
macro_rules! configure_compio {
    ($opts:expr, $builder:expr) => {{
        #[cfg(target_os = "linux")]
        {
            let num_entries = $opts.uring_entries.next_power_of_two();
            $builder.capacity(num_entries);

            if let Some(idle) = $opts.uring_sqpoll {
                $builder.sqpoll_idle(std::time::Duration::from_millis(idle as u64));
            } else {
                $builder.coop_taskrun(true).taskrun_flag(true);
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Default capacity when not on Linux since uring_entries is not available
            $builder.capacity(4096);
        }
    }};
}
