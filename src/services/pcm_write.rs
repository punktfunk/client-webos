//! Bounded recovery and frame-aligned retries for a blocking PCM device.
use std::time::Duration;

const MAX_RETRIES: usize = 4;
const MAX_UNDERRUNS: usize = 32;
const RETRY_DELAY: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteError {
    Interrupted,
    WouldBlock,
    Underrun,
    Device(i32),
    NoProgress,
    Stopped,
    InvalidCount,
}

pub fn write_all(
    pcm: &[i16],
    channels: usize,
    mut write: impl FnMut(&[i16]) -> Result<usize, WriteError>,
    mut prepare: impl FnMut() -> Result<(), WriteError>,
    stopped: impl Fn() -> bool,
    mut wait: impl FnMut(Duration),
) -> Result<(), WriteError> {
    if channels == 0 || pcm.len() % channels != 0 {
        return Err(WriteError::InvalidCount);
    }
    let mut remaining = pcm;
    let mut retries = 0;
    let mut underruns = 0;
    while !remaining.is_empty() {
        if stopped() {
            return Err(WriteError::Stopped);
        }
        let failure = match write(remaining) {
            Ok(0) => WriteError::WouldBlock,
            Ok(frames) if frames <= remaining.len() / channels => {
                remaining = &remaining[frames * channels..];
                retries = 0;
                underruns = 0;
                continue;
            }
            Ok(_) => return Err(WriteError::InvalidCount),
            Err(e) => e,
        };
        match failure {
            WriteError::Underrun => {
                if underruns == MAX_UNDERRUNS {
                    return Err(WriteError::NoProgress);
                }
                underruns += 1;
                prepare()?;
                continue;
            }
            WriteError::Interrupted | WriteError::WouldBlock => {}
            other => return Err(other),
        }
        if retries == MAX_RETRIES {
            return Err(WriteError::NoProgress);
        }
        retries += 1;
        // Bound consecutive stalls; positive writes reset both recovery budgets.
        wait(RETRY_DELAY);
    }
    Ok(())
}
