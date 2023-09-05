use indicatif::{MultiProgress, ProgressDrawTarget};
use std::io;
use std::sync::OnceLock;
use tracing_subscriber::fmt::MakeWriter;

/// Returns a global instance of [`indicatif::MultiProgress`].
///
/// Although you can always create an instance yourself any logging will interrupt pending
/// progressbars. To fix this issue, logging has been configured in such a way to it will not
/// interfere if you use the [`indicatif::MultiProgress`] returning by this function.
pub fn global_multi_progress() -> MultiProgress {
    static GLOBAL_MP: OnceLock<MultiProgress> = OnceLock::new();
    GLOBAL_MP
        .get_or_init(|| {
            let mp = MultiProgress::new();
            mp.set_draw_target(ProgressDrawTarget::stderr_with_hz(20));
            mp
        })
        .clone()
}

#[derive(Clone)]
pub struct IndicatifWriter {
    progress_bars: MultiProgress,
}

impl IndicatifWriter {
    pub(crate) fn new(pb: MultiProgress) -> Self {
        Self { progress_bars: pb }
    }
}

impl io::Write for IndicatifWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.progress_bars.suspend(|| io::stderr().write(buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.progress_bars.suspend(|| io::stderr().flush())
    }
}

impl<'a> MakeWriter<'a> for IndicatifWriter {
    type Writer = IndicatifWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
