//! Internal logging shim.
//!
//! Logs are fanned out to `tracing`, `log` and `defmt` if their respective
//! features are enabled (`defmt` also requires `std` to be off). Any
//! combination (or none) of the logging destinations may be enabled.
//!
//! Note that use of `defmt` imposes some restrictions on the logging format
//! at call sites. Only positional `{}` placeholders may be used; inline
//! captured identifiers (`{x}`) are not supported. Most display hints in
//! format specifiers are fine and supported by all transports. Note that when
//! logging from a `std`-gated module such as the `tokio` adapter, this
//! restriction does not apply since `defmt` can never be enabled when this
//! code is built.

#![allow(unused_macros, unused_imports)]

/// Suppress unused-variable warnings when no logging backend is enabled
macro_rules! discard {
    ($($arg:tt)+) => {
        if false {
            let _ = ::core::format_args!($($arg)+);
        }
    };
}

/// Dispatches one log record at `$level` (`trace`/`debug`/`info`/`warn`/`error`,
/// the method name shared by all three backends) to every enabled backend.
macro_rules! dispatch {
    ($level:ident, $($arg:tt)+) => {{
        #[cfg(feature = "tracing")]
        ::tracing::$level!($($arg)+);
        #[cfg(feature = "log")]
        ::log::$level!($($arg)+);
        #[cfg(all(feature = "defmt", not(feature = "std")))]
        ::defmt::$level!($($arg)+);
        #[cfg(not(any(
            feature = "tracing",
            feature = "log",
            all(feature = "defmt", not(feature = "std")),
        )))]
        $crate::log::discard!($($arg)+);
    }};
}

macro_rules! trace {
    ($($arg:tt)+) => { $crate::log::dispatch!(trace, $($arg)+) };
}

macro_rules! debug {
    ($($arg:tt)+) => { $crate::log::dispatch!(debug, $($arg)+) };
}

macro_rules! info {
    ($($arg:tt)+) => { $crate::log::dispatch!(info, $($arg)+) };
}

macro_rules! warning {
    ($($arg:tt)+) => { $crate::log::dispatch!(warn, $($arg)+) };
}

macro_rules! error {
    ($($arg:tt)+) => { $crate::log::dispatch!(error, $($arg)+) };
}

pub(crate) use {debug, discard, dispatch, error, info, trace, warning};
