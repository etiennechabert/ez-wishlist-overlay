//! The OCR engine seam: every consumer (the hideout pipeline's first pass,
//! box/stash scans, burst rounds, the unit-isolation tests) reaches the
//! engine through [`recognize_image`], so this module is the single place
//! where the implementation is chosen.
//!
//! Default: **PP-OCRv4 on ONNX Runtime** ([`super::engine_ppocr`]) — fully
//! local, CPU-only, no OS language pack, and a detector that survives the
//! busy item icons that blinded the previous engine (issue #181).
//!
//! `--features ocr-winrt` swaps back to the previous **Windows.Media.Ocr**
//! wrapper ([`super::engine_winrt`]) for A/B comparison while the migration
//! soaks; the fallback (and this dispatch) goes away once it has.

#[cfg(not(feature = "ocr-winrt"))]
pub use super::engine_ppocr::recognize_image;
#[cfg(feature = "ocr-winrt")]
pub use super::engine_winrt::recognize_image;
