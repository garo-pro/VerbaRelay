//! Catalog of Whisper models (ggml format, from the whisper.cpp model repository).

use serde::{Deserialize, Serialize};

/// Base URL of the official ggml model files.
pub const MODEL_BASE_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

/// Everything a peer needs to download and verify a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSpec {
    /// Short name, e.g. "large-v3-turbo".
    pub name: String,
    pub url: String,
    /// Lower-case hex SHA-256 of the file.
    pub sha256: String,
    /// File size in bytes.
    pub size: u64,
}

impl ModelSpec {
    /// File name used in the peer's model directory.
    pub fn file_name(&self) -> String {
        format!("ggml-{}.bin", sanitize(&self.name))
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// (name, sha256, size) of the built-in models.
const BUILT_IN: &[(&str, &str, u64)] = &[
    (
        "large-v3-turbo",
        "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
        1_624_555_275,
    ),
    (
        "large-v3-turbo-q8_0",
        "317eb69c11673c9de1e1f0d459b253999804ec71ac4c23c17ecf5fbe24e259a1",
        874_188_075,
    ),
    (
        "large-v3-turbo-q5_0",
        "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
        574_041_195,
    ),
    (
        "large-v3",
        "64d182b440b98d5203c4f9bd541544d84c605196c4f7b845dfa11fb23594d1e2",
        3_095_033_483,
    ),
    (
        "large-v3-q5_0",
        "d75795ecff3f83b5faa89d1900604ad8c780abd5739fae406de19f23ecd98ad1",
        1_081_140_203,
    ),
    (
        "medium",
        "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208",
        1_533_763_059,
    ),
    (
        "medium-q8_0",
        "42a1ffcbe4167d224232443396968db4d02d4e8e87e213d3ee2e03095dea6502",
        823_369_779,
    ),
    (
        "medium-q5_0",
        "19fea4b380c3a618ec4723c3eef2eb785ffba0d0538cf43f8f235e7b3b34220f",
        539_212_467,
    ),
    (
        "medium.en",
        "cc37e93478338ec7700281a7ac30a10128929eb8f427dda2e865faa8f6da4356",
        1_533_774_781,
    ),
    (
        "small",
        "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
        487_601_967,
    ),
    (
        "small-q8_0",
        "49c8fb02b65e6049d5fa6c04f81f53b867b5ec9540406812c643f177317f779f",
        264_464_607,
    ),
    (
        "small-q5_1",
        "ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb",
        190_085_487,
    ),
    (
        "small.en",
        "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d",
        487_614_201,
    ),
    (
        "base",
        "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe",
        147_951_465,
    ),
    (
        "base-q8_0",
        "c577b9a86e7e048a0b7eada054f4dd79a56bbfa911fbdacf900ac5b567cbb7d9",
        81_768_585,
    ),
    (
        "base-q5_1",
        "422f1ae452ade6f30a004d7e5c6a43195e4433bc370bf23fac9cc591f01a8898",
        59_707_625,
    ),
    (
        "base.en",
        "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
        147_964_211,
    ),
    (
        "tiny",
        "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21",
        77_691_713,
    ),
    (
        "tiny-q8_0",
        "c2085835d3f50733e2ff6e4b41ae8a2b8d8110461e18821b09a15c40c42d1cca",
        43_537_433,
    ),
    (
        "tiny-q5_1",
        "818710568da3ca15689e31a743197b520007872ff9576237bda97bd1b469c3d7",
        32_152_673,
    ),
    (
        "tiny.en",
        "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f",
        77_704_715,
    ),
];

/// The model used when nothing else is configured.
pub const DEFAULT_MODEL: &str = "large-v3-turbo";

/// The small model peers use to compare backends during setup.
pub const PROBE_MODEL: &str = "tiny";

/// All built-in models.
pub fn catalog() -> Vec<ModelSpec> {
    BUILT_IN
        .iter()
        .map(|(name, sha256, size)| ModelSpec {
            name: (*name).to_string(),
            url: format!("{MODEL_BASE_URL}/ggml-{name}.bin"),
            sha256: (*sha256).to_string(),
            size: *size,
        })
        .collect()
}

/// Looks up a built-in model by name.
pub fn find(name: &str) -> Option<ModelSpec> {
    catalog().into_iter().find(|m| m.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_consistent() {
        let models = catalog();
        assert!(find(DEFAULT_MODEL).is_some());
        assert!(find(PROBE_MODEL).is_some());
        for m in &models {
            assert_eq!(m.sha256.len(), 64, "{}", m.name);
            assert!(m.sha256.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(m.url.ends_with(&m.file_name()));
        }
    }

    #[test]
    fn file_names_are_safe() {
        let spec = ModelSpec {
            name: "../evil".into(),
            url: String::new(),
            sha256: String::new(),
            size: 0,
        };
        assert_eq!(spec.file_name(), "ggml-.._evil.bin");
        assert!(!spec.file_name().contains('/'));
    }
}
