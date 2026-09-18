//! whisper.cpp, through the four functions `vendor/whisper.cpp/engram_whisper.h`
//! declares. The only `unsafe` in the speech path is in this file.
//!
//! It runs over the ggml that llama.cpp built — see `build.rs` — so there is
//! nothing to initialise here and nothing to keep in step at run time: the
//! matching was done when the two were compiled together.

use crate::error::{Error, Result};
use std::ffi::{CStr, CString, c_char, c_float, c_int};
use std::path::Path;

#[repr(C)]
struct Raw {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn engram_whisper_open(path: *const c_char) -> *mut Raw;
    fn engram_whisper_run(
        w: *mut Raw,
        samples: *const c_float,
        n: c_int,
        lang: *const c_char,
        threads: c_int,
    ) -> *mut c_char;
    fn engram_whisper_free_text(text: *mut c_char);
    fn engram_whisper_close(w: *mut Raw);
}

fn failed(detail: impl std::fmt::Display) -> Error {
    Error::Inference {
        role: "transcribe",
        detail: detail.to_string(),
    }
}

/// One loaded speech model. It owns a whisper context, which may move between
/// threads and may not be used from two at once: `Send`, and `run` takes
/// `&mut self`.
pub struct Whisper(*mut Raw);

// The context has no thread affinity; whisper.cpp only forbids concurrent use,
// which `&mut self` already does.
unsafe impl Send for Whisper {}

impl Whisper {
    pub fn open(path: &Path) -> Result<Whisper> {
        let c = CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| failed("the model's path has a NUL in it"))?;
        // Safety: a valid NUL-terminated string, read only for the call.
        let raw = unsafe { engram_whisper_open(c.as_ptr()) };
        if raw.is_null() {
            return Err(failed(format!(
                "{} is not a speech model this build reads",
                path.display()
            )));
        }
        Ok(Whisper(raw))
    }

    /// The words in `samples`, which are 16 kHz mono in ±1. `lang` is an
    /// ISO-639-1 code, or `None` to let the model decide from the audio.
    pub fn run(&mut self, samples: &[f32], lang: Option<&str>, threads: i32) -> Result<String> {
        let n = c_int::try_from(samples.len()).map_err(|_| failed("the recording is too long"))?;
        let lang = lang
            .map(CString::new)
            .transpose()
            .map_err(|_| failed("the language code has a NUL in it"))?;
        // Safety: `self.0` is a live context (non-null since `open`, freed
        // only in `drop`); `samples` and `lang` outlive the call, which
        // retains neither.
        let text = unsafe {
            engram_whisper_run(
                self.0,
                samples.as_ptr(),
                n,
                lang.as_ref().map_or(std::ptr::null(), |l| l.as_ptr()),
                threads,
            )
        };
        if text.is_null() {
            return Err(failed("whisper could not read this recording"));
        }
        // Safety: a NUL-terminated string the shim malloc'd, freed here and
        // nowhere else.
        let words = unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .trim()
            .to_string();
        unsafe { engram_whisper_free_text(text) };
        Ok(words)
    }
}

impl Drop for Whisper {
    fn drop(&mut self) {
        // Safety: the one owner, and the last use.
        unsafe { engram_whisper_close(self.0) };
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A file the speech tests need, or `None` with a line saying so. Gated the
    /// way `local`'s model-backed tests are: one variable turns them all on.
    pub(crate) fn file(name: &str) -> Option<std::path::PathBuf> {
        let path = std::env::var_os("ENGRAM_TEST_MODELS")
            .map(|d| std::path::PathBuf::from(d).join(name))
            .filter(|p| p.exists());
        if path.is_none() {
            eprintln!("skipped: no {name} under ENGRAM_TEST_MODELS");
        }
        path
    }

    #[test]
    fn a_known_recording_comes_back_as_its_words() {
        let (Some(model), Some(wav)) = (file("speech.bin"), file("speech.wav")) else {
            return;
        };
        let samples: Vec<f32> = hound::WavReader::open(wav)
            .unwrap()
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect();
        let mut w = Whisper::open(&model).unwrap();
        let words = w.run(&samples, Some("en"), 4).unwrap().to_lowercase();
        assert!(words.contains("ask not what your country"), "{words}");
        // And again on the same context: a model is loaded once per recording
        // today, but nothing about it is single-use.
        let again = w.run(&samples, None, 4).unwrap().to_lowercase();
        assert!(again.contains("ask not what your country"), "{again}");
    }

    #[test]
    fn a_file_that_is_not_a_model_is_an_error_and_not_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-model.bin");
        std::fs::write(&path, b"this is not ggml").unwrap();
        assert!(Whisper::open(&path).is_err());
        assert!(Whisper::open(&dir.path().join("absent.bin")).is_err());
    }
}
