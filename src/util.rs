use std::time::Duration;

/// Formats a `Duration` with humantime, truncated to whole seconds.
///
/// `humantime::format_duration` prints down to nanoseconds by default (e.g.
/// "27m 45s 974ms 322us 966ns"), which is far more precision than any of our
/// elapsed-time/ETA displays need and wide enough to wrap table columns --
/// truncate to whole seconds first.
pub trait HumantimeSecs {
    fn humantime_secs(&self) -> humantime::FormattedDuration;
}

impl HumantimeSecs for Duration {
    fn humantime_secs(&self) -> humantime::FormattedDuration {
        humantime::format_duration(Duration::from_secs(self.as_secs()))
    }
}
