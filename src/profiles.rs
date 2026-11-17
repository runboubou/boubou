use anyhow::{Context, Result, ensure};
use clap::builder::{PossibleValuesParser, TypedValueParser};
use serde::Deserialize;
use std::{path::Path, sync::OnceLock};

/// Complete English speech pipelines, shared with asset locking and the packaged runner.
#[derive(Deserialize)]
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    pub guided: bool,
    pub tts: &'static str,
    pub stt: &'static str,
    pub llm: &'static str,
    pub recommended_memory_gib: u64,
    pub memory_gib: u64,
}

pub fn presets() -> &'static [Preset] {
    static PRESETS: OnceLock<Vec<Preset>> = OnceLock::new();
    PRESETS.get_or_init(|| {
        serde_json::from_str(include_str!("../packaging/qwen/runtimes.json"))
            .expect("valid bundled runtime catalog")
    })
}

#[derive(Debug, Clone, Copy)]
pub struct Download<'a> {
    pub name: &'a str,
    pub url: &'a str,
    pub size: u64,
    pub sha256: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub struct Runtime {
    pub label: &'static str,
    /// The `std::env::consts` OS and architecture this build runs on.
    pub host: (&'static str, &'static str),
    pub archive: Download<'static>,
    pub directory: &'static str,
    pub server: &'static str,
}

const LLAMA_CPP_MACOS: Runtime = Runtime {
    label: "llama.cpp b11960",
    host: ("macos", "aarch64"),
    archive: Download {
        name: "llama-b11960-bin-macos-arm64.tar.gz",
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b11960/llama-b11960-bin-macos-arm64.tar.gz",
        size: 16_465_074,
        sha256: "58e5a5e1d7c2bf66cedd9c29e0323f3e570660a70a1e2156d32eed08f4cc1e3c",
    },
    directory: "runtimes/b11960-macos-arm64",
    server: "llama-b11960/llama-server",
};

/// Linux uses upstream's portable CPU builds; GPU builds need per-driver testing.
const LLAMA_CPP: &[Runtime] = &[
    LLAMA_CPP_MACOS,
    Runtime {
        host: ("linux", "x86_64"),
        archive: Download {
            name: "llama-b11960-bin-ubuntu-x64.tar.gz",
            url: "https://github.com/ggml-org/llama.cpp/releases/download/b11960/llama-b11960-bin-ubuntu-x64.tar.gz",
            size: 22_686_210,
            sha256: "31cc3a6bc638ac6c48dc4b1b74888f1eaadf3d14227c1e5f67daf9a49541d97a",
        },
        directory: "runtimes/b11960-linux-x64",
        ..LLAMA_CPP_MACOS
    },
    Runtime {
        host: ("linux", "aarch64"),
        archive: Download {
            name: "llama-b11960-bin-ubuntu-arm64.tar.gz",
            url: "https://github.com/ggml-org/llama.cpp/releases/download/b11960/llama-b11960-bin-ubuntu-arm64.tar.gz",
            size: 18_411_465,
            sha256: "0065ecfbffe8a790bb0d28059a3320cdc8f723b7f3f248cdc8bfc04790927715",
        },
        directory: "runtimes/b11960-linux-arm64",
        ..LLAMA_CPP_MACOS
    },
];

// Use PCM16 SSE; the older English archive emits float32 audio.
const LIQUID_AUDIO: &[Runtime] = &[
    Runtime {
        label: "Liquid Audio 8256 (22e8cd6f6)",
        host: ("macos", "aarch64"),
        archive: Download {
            name: "liquid-audio-jp-64b96718b341dbd5650f9e85627cecdcbd4ac61b-macos-arm64.zip",
            url: "https://huggingface.co/LiquidAI/LFM2.5-Audio-1.5B-JP-GGUF/resolve/64b96718b341dbd5650f9e85627cecdcbd4ac61b/runners/llama-liquid-audio-macos-arm64.zip",
            size: 7_280_738,
            sha256: "e59b7be254b6386960fc1d3cf51851709220202a4fdf7f83eb29953987992c9b",
        },
        directory: "runtimes/liquid-audio-jp-64b96718b341dbd5650f9e85627cecdcbd4ac61b-macos-arm64",
        server: "llama-liquid-audio-macos-arm64/llama-liquid-audio-server",
    },
    Runtime {
        label: "Liquid Audio 8256 (22e8cd6f6)",
        host: ("linux", "x86_64"),
        archive: Download {
            name: "liquid-audio-jp-64b96718b341dbd5650f9e85627cecdcbd4ac61b-ubuntu-x64.zip",
            url: "https://huggingface.co/LiquidAI/LFM2.5-Audio-1.5B-JP-GGUF/resolve/64b96718b341dbd5650f9e85627cecdcbd4ac61b/runners/llama-liquid-audio-ubuntu-x64.zip",
            size: 14_127_918,
            sha256: "1104d54e8bca5faabd6de28d27f17283a4b7ef464f0ab6c0f1f5eb9a5a143e91",
        },
        directory: "runtimes/liquid-audio-jp-64b96718b341dbd5650f9e85627cecdcbd4ac61b-linux-x64",
        server: "llama-liquid-audio-ubuntu-x64/llama-liquid-audio-server",
    },
    Runtime {
        label: "Liquid Audio 8256 (22e8cd6f6)",
        host: ("linux", "aarch64"),
        archive: Download {
            name: "liquid-audio-jp-64b96718b341dbd5650f9e85627cecdcbd4ac61b-ubuntu-arm64.zip",
            url: "https://huggingface.co/LiquidAI/LFM2.5-Audio-1.5B-JP-GGUF/resolve/64b96718b341dbd5650f9e85627cecdcbd4ac61b/runners/llama-liquid-audio-ubuntu-arm64.zip",
            size: 9_243_426,
            sha256: "c04db9ffb88bd9648d89a875814a52a71f763132cee858bb0dad200ebcc47357",
        },
        directory: "runtimes/liquid-audio-jp-64b96718b341dbd5650f9e85627cecdcbd4ac61b-linux-arm64",
        server: "llama-liquid-audio-ubuntu-arm64/llama-liquid-audio-server",
    },
];

#[derive(Debug, Clone, Copy)]
pub enum Protocol {
    Realtime,
    LiquidAudio {
        tokenizer: Download<'static>,
        vocoder: Download<'static>,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct Profile {
    pub id: &'static str,
    pub model_directory: &'static str,
    pub quantization: &'static str,
    pub voice_chat_language: &'static str,
    pub model: Download<'static>,
    pub projector: Download<'static>,
    pub license: Download<'static>,
    pub runtimes: &'static [Runtime],
    pub protocol: Protocol,
    /// Verified runtime executable for local image/video questions, when supported.
    pub media_cli: Option<&'static str>,
    /// Conservative runtime memory budget, including headroom; None excludes recommendations.
    pub memory_budget: Option<u64>,
    pub context_tokens: usize,
    pub max_output_tokens: usize,
    pub audio_samples_per_token: usize,
    pub instructions: &'static str,
    pub backend_args: &'static [&'static str],
}

impl Profile {
    /// The pinned engine build for this computer.
    pub fn runtime(&self) -> Result<&'static Runtime> {
        let host = (std::env::consts::OS, std::env::consts::ARCH);
        self.runtimes
            .iter()
            .find(|runtime| runtime.host == host)
            .with_context(|| {
                format!(
                    "{} runs on Apple Silicon Macs and x86-64 or arm64 Linux.",
                    self.id
                )
            })
    }

    pub fn model_assets(&self) -> impl Iterator<Item = &Download<'static>> {
        [&self.license, &self.model, &self.projector]
            .into_iter()
            .chain(
                match &self.protocol {
                    Protocol::Realtime => None,
                    Protocol::LiquidAudio { tokenizer, vocoder } => Some([tokenizer, vocoder]),
                }
                .into_iter()
                .flatten(),
            )
    }
}

pub static DEFAULT: Profile = Profile {
    id: "LFM3-Audio-2B",
    model_directory: "models/lfm3-audio-2b/02355c1a141e464a064f2ab2c276638dbcb7f38a",
    quantization: "Q4_K_M + Q8_0 audio",
    voice_chat_language: "English",
    model: Download {
        name: "LFM3-Audio-2B-Q4_K_M.gguf",
        url: "https://huggingface.co/LiquidAI/LFM3-Audio-2B-GGUF/resolve/02355c1a141e464a064f2ab2c276638dbcb7f38a/LFM3-Audio-2B-Q4_K_M.gguf",
        size: 1_099_626_016,
        sha256: "9a773289bd321ed879cd3a76545f6aca506e403851a45fb315420aee45625be2",
    },
    projector: Download {
        name: "mmproj-LFM3-Audio-2B-Q8_0.gguf",
        url: "https://huggingface.co/LiquidAI/LFM3-Audio-2B-GGUF/resolve/02355c1a141e464a064f2ab2c276638dbcb7f38a/mmproj-LFM3-Audio-2B-Q8_0.gguf",
        size: 303_811_840,
        sha256: "2b6648b1fd48af16aa0877b0592b144d15070783673e038a379fdbc51d40e365",
    },
    license: Download {
        name: "LICENSE",
        url: "https://huggingface.co/LiquidAI/LFM3-Audio-2B-GGUF/resolve/02355c1a141e464a064f2ab2c276638dbcb7f38a/LICENSE",
        size: 10_574,
        sha256: "4d28ca14dedc0b3d0fcc2b3339f0e79931faa33874f3d24f522183a8fc70068c",
    },
    runtimes: LLAMA_CPP,
    protocol: Protocol::Realtime,
    media_cli: None,
    // Native voice-file/cancellation profiling used about 2 GiB RSS. Reserve 6 GiB
    // for the full runtime and headroom; this is an estimate, not a speed guarantee.
    memory_budget: Some(6 * 1024 * 1024 * 1024),
    context_tokens: 32_768,
    max_output_tokens: 1024,
    // The pinned LFM3 encoder uses one context position per eight 160-sample mel frames.
    audio_samples_per_token: 1280,
    // Keep Liquid's voice-chat prompt first, followed by the accepted long-session guidance.
    instructions: "Respond with interleaved text and audio.\n\nYou are a friendly voice assistant in a spoken conversation. Answer the user's latest words in one to three short sentences. Do not repeat an earlier reply. If the user only acknowledges you, keep the conversation going naturally.",
    // Greedy text decoding loops long replies; audio frames retain the model's own sampler.
    backend_args: &[
        "--temp",
        "0.3",
        "--min-p",
        "0.15",
        "--top-k",
        "0",
        "--top-p",
        "1.0",
        "--repeat-penalty",
        "1.05",
    ],
};

pub static OMNI: Profile = Profile {
    id: "LFM3-Omni-2B",
    model_directory: "models/lfm3-omni-2b/d508b88f08a5b4286f2d2805a6a8d69a0ff44c72",
    model: Download {
        name: "LFM3-Omni-2B-Q4_K_M.gguf",
        url: "https://huggingface.co/LiquidAI/LFM3-Omni-2B-GGUF/resolve/d508b88f08a5b4286f2d2805a6a8d69a0ff44c72/LFM3-Omni-2B-Q4_K_M.gguf",
        size: 1_099_630_080,
        sha256: "9f8d30e9d4fd7a9349d586e077988229b3ca0a0e4ad730897563d1eccc9893e2",
    },
    projector: Download {
        name: "mmproj-LFM3-Omni-2B-Q8_0.gguf",
        url: "https://huggingface.co/LiquidAI/LFM3-Omni-2B-GGUF/resolve/d508b88f08a5b4286f2d2805a6a8d69a0ff44c72/mmproj-LFM3-Omni-2B-Q8_0.gguf",
        size: 506_842_528,
        sha256: "8a25f9d1931244429de404fccacfb7fb933970a36aa5688d2f5069f29c6b27b1",
    },
    license: Download {
        url: "https://huggingface.co/LiquidAI/LFM3-Omni-2B-GGUF/resolve/d508b88f08a5b4286f2d2805a6a8d69a0ff44c72/LICENSE",
        ..DEFAULT.license
    },
    media_cli: Some("llama-b11960/llama-mtmd-cli"),
    // Omni is non-reasoning; audio sampling defaults are embedded in its GGUF.
    backend_args: &["--reasoning", "off", "--temp", "0"],
    // Both LFM3 profiles use the same Realtime codec, context and voice-chat prompt.
    ..DEFAULT
};

/// Recall across turns fails whenever the newest message is spoken; see the README model list.
pub static LFM25: Profile = Profile {
    id: "LFM2.5-Audio-1.5B",
    model_directory: "models/lfm2.5-audio-1.5b/7d525f883a077e20afb782f2ff618edcae0e39e4",
    quantization: "Q4_0 + Q8_0 audio",
    voice_chat_language: "English",
    model: Download {
        name: "LFM2.5-Audio-1.5B-Q4_0.gguf",
        url: "https://huggingface.co/LiquidAI/LFM2.5-Audio-1.5B-GGUF/resolve/7d525f883a077e20afb782f2ff618edcae0e39e4/LFM2.5-Audio-1.5B-Q4_0.gguf",
        size: 695_750_880,
        sha256: "3583bee853be20331ca342b0593fefd8acc43fb61a41ec6f1a1dc7465823e0d8",
    },
    projector: Download {
        name: "mmproj-LFM2.5-Audio-1.5B-Q8_0.gguf",
        url: "https://huggingface.co/LiquidAI/LFM2.5-Audio-1.5B-GGUF/resolve/7d525f883a077e20afb782f2ff618edcae0e39e4/mmproj-LFM2.5-Audio-1.5B-Q8_0.gguf",
        size: 293_443_936,
        sha256: "8f943619aa95cfb6218347ffd97edc4c485e76b6a839c899a4d9617db1939c67",
    },
    license: Download {
        name: "LICENSE",
        url: "https://huggingface.co/LiquidAI/LFM2.5-Audio-1.5B-GGUF/resolve/7d525f883a077e20afb782f2ff618edcae0e39e4/LICENSE",
        size: 10_596,
        sha256: "5188f2b355da20647257a3156db5834c794e5fb5e6d8dc4d4cdbb3180e75b85b",
    },
    runtimes: LIQUID_AUDIO,
    protocol: Protocol::LiquidAudio {
        tokenizer: Download {
            name: "tokenizer-LFM2.5-Audio-1.5B-Q8_0.gguf",
            url: "https://huggingface.co/LiquidAI/LFM2.5-Audio-1.5B-GGUF/resolve/7d525f883a077e20afb782f2ff618edcae0e39e4/tokenizer-LFM2.5-Audio-1.5B-Q8_0.gguf",
            size: 76_957_632,
            sha256: "74f7f648699dcd819bb1eca8e80dfeed5a83684395c443d876b25bb3bcac15e3",
        },
        vocoder: Download {
            name: "vocoder-LFM2.5-Audio-1.5B-Q8_0.gguf",
            url: "https://huggingface.co/LiquidAI/LFM2.5-Audio-1.5B-GGUF/resolve/7d525f883a077e20afb782f2ff618edcae0e39e4/vocoder-LFM2.5-Audio-1.5B-Q8_0.gguf",
            size: 205_742_272,
            sha256: "3063b8ebfe08e231353506a23e765a4016db31fbb9cdd2881f3283a390313f23",
        },
    },
    // Direct selection is available; automatic recommendations require memory profiling.
    memory_budget: None,
    context_tokens: 8192,
    // This runner accepts only its exact, predefined task prompts.
    instructions: "Respond with interleaved text and audio.",
    // The runner owns text sampling; matching GGUF assets own audio interleaving/sampling.
    backend_args: &[],
    ..DEFAULT
};

pub static PROFILES: &[&Profile] = &[&DEFAULT, &OMNI, &LFM25];

/// A packaged voice pipeline and a Liquid server have different launch contracts.
#[derive(Clone, Copy, Debug)]
pub enum Model {
    Liquid(&'static Profile),
    Modular(&'static str),
}

/// Who speaks, a Qwen3-TTS CustomVoice speaker, a Kokoro, KittenTTS or Supertonic voice or a
/// voice cloned on this computer, and the language Boubou talks with it in.
pub type Voice = (String, &'static str);

/// Kokoro-82M's voices. The first letter of a name is the language Kokoro speaks with it, as
/// `KOKORO_LANGUAGES` names them.
pub const KOKORO_VOICES: [&str; 54] = [
    "af_heart",
    "af_alloy",
    "af_aoede",
    "af_bella",
    "af_jessica",
    "af_kore",
    "af_nicole",
    "af_nova",
    "af_river",
    "af_sarah",
    "af_sky",
    "am_adam",
    "am_echo",
    "am_eric",
    "am_fenrir",
    "am_liam",
    "am_michael",
    "am_onyx",
    "am_puck",
    "am_santa",
    "bf_alice",
    "bf_emma",
    "bf_isabella",
    "bf_lily",
    "bm_daniel",
    "bm_fable",
    "bm_george",
    "bm_lewis",
    "ef_dora",
    "em_alex",
    "em_santa",
    "ff_siwis",
    "hf_alpha",
    "hf_beta",
    "hm_omega",
    "hm_psi",
    "if_sara",
    "im_nicola",
    "jf_alpha",
    "jf_gongitsune",
    "jf_nezumi",
    "jf_tebukuro",
    "jm_kumo",
    "pf_dora",
    "pm_alex",
    "pm_santa",
    "zf_xiaobei",
    "zf_xiaoni",
    "zf_xiaoxiao",
    "zf_xiaoyi",
    "zm_yunjian",
    "zm_yunxi",
    "zm_yunxia",
    "zm_yunyang",
];

/// KittenTTS's voices, all English: the names 0.8 checkpoints give them, and the IDs every
/// checkpoint has.
pub const KITTEN_VOICES: [(&str, &str); 8] = [
    ("Bella", "expr-voice-2-f"),
    ("Jasper", "expr-voice-2-m"),
    ("Luna", "expr-voice-3-f"),
    ("Bruno", "expr-voice-3-m"),
    ("Rosie", "expr-voice-4-f"),
    ("Hugo", "expr-voice-4-m"),
    ("Kiki", "expr-voice-5-f"),
    ("Leo", "expr-voice-5-m"),
];

/// Supertonic 3's voice styles, five male and five female. Each speaks every language.
pub const SUPERTONIC_VOICES: [&str; 10] =
    ["M1", "M2", "M3", "M4", "M5", "F1", "F2", "F3", "F4", "F5"];

/// The languages Supertonic 3 speaks and Parakeet hears, by code. Supertonic also speaks Arabic,
/// Hindi, Indonesian, Japanese, Korean, Turkish and Vietnamese, which Parakeet does not hear.
pub const SUPERTONIC_LANGUAGES: [(&str, &str); 24] = [
    ("en", "English"),
    ("bg", "Bulgarian"),
    ("hr", "Croatian"),
    ("cs", "Czech"),
    ("da", "Danish"),
    ("nl", "Dutch"),
    ("et", "Estonian"),
    ("fi", "Finnish"),
    ("fr", "French"),
    ("de", "German"),
    ("el", "Greek"),
    ("hu", "Hungarian"),
    ("it", "Italian"),
    ("lv", "Latvian"),
    ("lt", "Lithuanian"),
    ("pl", "Polish"),
    ("pt", "Portuguese"),
    ("ro", "Romanian"),
    ("ru", "Russian"),
    ("sk", "Slovak"),
    ("sl", "Slovenian"),
    ("es", "Spanish"),
    ("sv", "Swedish"),
    ("uk", "Ukrainian"),
];

/// Pocket TTS's preset voices. Every stock config has a voice state for each, which speaks the
/// config's language.
pub const POCKET_VOICES: [&str; 27] = [
    "alba",
    "marius",
    "javert",
    "jean",
    "fantine",
    "cosette",
    "eponine",
    "azelma",
    "anna",
    "vera",
    "charles",
    "paul",
    "george",
    "mary",
    "jane",
    "michael",
    "eve",
    "bill_boerst",
    "peter_yearsley",
    "stuart_bell",
    "caro_davy",
    "giovanni",
    "lola",
    "juergen",
    "rafael",
    "daan",
    "estelle",
];

/// The language of each Pocket TTS stock config, by the word its name starts with, and the preset
/// voice the config starts with.
const POCKET_LANGUAGES: [(&str, &str, &str); 7] = [
    ("english", "English", "alba"),
    ("dutch", "Dutch", "daan"),
    ("french", "French", "estelle"),
    ("german", "German", "juergen"),
    ("italian", "Italian", "giovanni"),
    ("portuguese", "Portuguese", "rafael"),
    ("spanish", "Spanish", "lola"),
];

/// The language a Pocket TTS profile speaks, and the voice it starts with.
pub fn pocket(id: &str) -> Option<(&'static str, &'static str)> {
    let config = id.strip_prefix("pocket-tts:")?;
    let (_, speaks, voice) = POCKET_LANGUAGES
        .into_iter()
        .find(|(word, ..)| config.split('_').next() == Some(*word))?;
    Some((speaks, voice))
}

/// The English voices of Phonon's voice catalog, which its English checkpoint speaks with.
pub const PHONON_VOICES: [&str; 37] = [
    "Zoey",
    "Juniper",
    "Everly",
    "Harper",
    "Hadley",
    "Delia",
    "Easton",
    "Micah",
    "Trevor",
    "Russell",
    "Preston",
    "Miles",
    "Maeve",
    "Freya",
    "Pippa",
    "Elodie-Rose",
    "Imogen",
    "Lottie",
    "Alfie",
    "Hugh",
    "Edwin",
    "Spencer",
    "Toby",
    "Teddy",
    "Bryony",
    "Captain Silas Blackwater",
    "Morwenna of the Deep",
    "Old Mortlake",
    "Azgorath",
    "Fennick the Young Elf",
    "The Thing Under the Stairs",
    "Vyrmathrax the Ancient",
    "Grub the Ogre",
    "Master Aldric",
    "The Snow Queen",
    "Sir Bramwell",
    "Fairy Godmother",
];

/// Kokoro's language codes, and whether Parakeet hears the language: American and British
/// English, Spanish, French, Hindi, Italian, Japanese, Brazilian Portuguese and Mandarin.
const KOKORO_LANGUAGES: [(char, &str, bool); 9] = [
    ('a', "English", true),
    ('b', "English", true),
    ('e', "Spanish", true),
    ('f', "French", true),
    ('h', "Hindi", false),
    ('i', "Italian", true),
    ('j', "Japanese", false),
    ('p', "Portuguese", true),
    ('z', "Chinese", false),
];

/// Qwen3-TTS CustomVoice's speakers, each with its own language. Each also speaks the other
/// languages of Qwen3-TTS, which `--language` names.
pub const VOICES: [(&str, &str); 9] = [
    ("Aiden", "English"),
    ("Ryan", "English"),
    ("Ono_Anna", "Japanese"),
    ("Sohee", "Korean"),
    ("Vivian", "Chinese"),
    ("Serena", "Chinese"),
    ("Uncle_Fu", "Chinese"),
    ("Dylan", "Chinese"),
    ("Eric", "Chinese"),
];

/// Profiles served by the packaged speech-to-speech runner. Pocket TTS profiles name a stock config.
pub const MODULAR_IDS: &[&str] = &[
    "qwen3-tts:1.7b-customvoice",
    "qwen3-tts:0.6b-customvoice",
    "qwen3-tts:1.7b-customvoice-whisper",
    "qwen3-tts:0.6b-customvoice-whisper",
    "kokoro:82m",
    "kokoro:82m-whisper",
    "phonon:31m-en",
    "supertonic:3",
    "chattts:en",
    "chattts:whisper",
    "mms-tts:eng",
    "omnivoice:0.6b",
    "pocket-tts:english_2026-10",
    "pocket-tts:english_2026-09_2",
    "pocket-tts:english_2026-09",
    "pocket-tts:english_2026-09_24l",
    "pocket-tts:english_drifting_26-09",
    "pocket-tts:english_2026-04",
    "pocket-tts:english_2026-04_24l",
    "pocket-tts:english_2026-01",
    "pocket-tts:english_18m_2026-10",
    "pocket-tts:english_18m_refl_2026-10",
    "pocket-tts:english_7m_2026-10",
    "pocket-tts:english_7m_refl_2026-10",
    "pocket-tts:english_2m_2026-10",
    "pocket-tts:english_2m_refl_2026-10",
    "pocket-tts:dutch",
    "pocket-tts:dutch_24l",
    "pocket-tts:french",
    "pocket-tts:french_24l",
    "pocket-tts:german",
    "pocket-tts:german_24l",
    "pocket-tts:italian",
    "pocket-tts:italian_24l",
    "pocket-tts:portuguese",
    "pocket-tts:portuguese_24l",
    "pocket-tts:spanish",
    "pocket-tts:spanish_24l",
    "kitten-tts:mini-0.8",
    "kitten-tts:micro-0.8",
    "kitten-tts:nano-0.8-fp32",
    "kitten-tts:nano-0.8-int8",
    "kitten-tts:mini-0.1",
    "kitten-tts:nano-0.1",
    "kitten-tts:nano-0.2",
];

impl Model {
    pub fn id(self) -> &'static str {
        match self {
            Self::Liquid(profile) => profile.id,
            Self::Modular(id) => id,
        }
    }

    pub fn all() -> impl Iterator<Item = Self> {
        let packaged = crate::modular::packaged();
        // The CUDA runner packages Parakeet and Qwen3.5-4B only: no Whisper entries and none of
        // the macOS presets with other model sizes. Qwen3-TTS waits for upstream RTX 50 support,
        // and ChatTTS, which takes about 9 s for its first sentence, stays on the Mac.
        let linux = cfg!(target_os = "linux");
        let modular = MODULAR_IDS.iter().copied().filter(move |&id| {
            packaged
                && !(linux
                    && (crate::modular::whisper(id)
                        || id.starts_with("qwen3-tts:")
                        || id.starts_with("chattts:")))
        });
        let presets = presets()
            .iter()
            .filter(move |preset| packaged && !linux && !MODULAR_IDS.contains(&preset.id))
            .map(|preset| preset.id);
        PROFILES
            .iter()
            .copied()
            .map(Self::Liquid)
            .chain(modular.chain(presets).map(Self::Modular))
    }

    pub fn find(id: &str) -> Option<Self> {
        Self::all().find(|model| model.id() == id)
    }

    /// Boubou's estimate of the memory, in GiB, this model needs for speech and conversation,
    /// where it has one. A Whisper entry takes its English preset's: measured after a short
    /// conversation, its runner peaked at 9.1 GB against the preset's 10 GB.
    pub fn memory_gib(self) -> Option<u64> {
        match self {
            Self::Liquid(profile) => profile.memory_budget.map(|bytes| bytes >> 30),
            Self::Modular(id) => presets()
                .iter()
                .find(|preset| preset.id == id.strip_suffix("-whisper").unwrap_or(id))
                .map(|preset| preset.memory_gib),
        }
    }

    /// This model speaking as the voice `--voice` names, in the language `--language` names: on
    /// Qwen3-TTS one of `VOICES` or of the voices cloned in `cache`, on Kokoro one of
    /// `KOKORO_VOICES`, on KittenTTS one of `KITTEN_VOICES`, on Supertonic one of
    /// `SUPERTONIC_VOICES`, on Pocket TTS one of `POCKET_VOICES`, which speaks the config's
    /// language, and on Phonon one of `PHONON_VOICES`. Qwen3-TTS and Supertonic voices speak several languages, their own
    /// unless one is named. A language Parakeet does not hear brings the model's Whisper entry,
    /// which hears that language.
    pub fn voiced(
        self,
        voice: Option<&str>,
        language: Option<&str>,
        cache: &Path,
    ) -> Result<(Self, Option<Voice>)> {
        if voice.is_none() && language.is_none() {
            return Ok((self, None));
        }
        let id = self.id();
        // The language `--language` names among those a model speaks, by its code or its name.
        let named = |spoken: &[(&str, &'static str)]| {
            let Some(language) = language else {
                return Ok(None);
            };
            let found = spoken.iter().find(|(code, title)| {
                code.eq_ignore_ascii_case(language) || title.eq_ignore_ascii_case(language)
            });
            let codes: Vec<_> = spoken.iter().map(|(code, _)| *code).collect();
            let family = id.split(':').next().unwrap_or(id);
            found.map(|(_, title)| Some(*title)).with_context(|| {
                format!(
                    "{family} does not speak {language}. Choose from: {}.",
                    codes.join(", ")
                )
            })
        };
        if id.starts_with("supertonic:") {
            // M1 is the voice the runner starts with.
            let name = voice.map_or(Ok("M1"), |voice| {
                let found = SUPERTONIC_VOICES
                    .into_iter()
                    .find(|name| name.eq_ignore_ascii_case(voice));
                found.with_context(|| {
                    format!(
                        "No Supertonic voice is named {voice}. Choose from: {}.",
                        SUPERTONIC_VOICES.join(", ").to_lowercase()
                    )
                })
            })?;
            let speaks = named(&SUPERTONIC_LANGUAGES)?.unwrap_or("English");
            return Ok((self, Some((name.to_owned(), speaks))));
        }
        ensure!(
            language.is_none() || id.starts_with("qwen3-tts:"),
            "{id} speaks one language, or its voice's own. Qwen3-TTS and Supertonic models take --language."
        );
        // Without --voice, the speaker each Qwen3-TTS entry starts with.
        let voice = voice.unwrap_or(if crate::modular::whisper(id) {
            "Vivian"
        } else {
            "Aiden"
        });
        if id.starts_with("pocket-tts:") {
            let name = POCKET_VOICES
                .into_iter()
                .find(|name| name.eq_ignore_ascii_case(voice))
                .with_context(|| {
                    format!(
                        "No Pocket TTS voice is named {voice}. Choose from: {}.",
                        POCKET_VOICES.join(", ")
                    )
                })?;
            let (speaks, _) = pocket(id).context("Unknown Pocket TTS language")?;
            return Ok((self, Some((name.to_owned(), speaks))));
        }
        if id.starts_with("phonon:") {
            // Some catalog names have spaces. A command can name them with dashes instead.
            let slug = |name: &str| name.to_lowercase().replace([' ', '_'], "-");
            let wanted = slug(voice);
            let name = PHONON_VOICES
                .into_iter()
                .find(|name| slug(name) == wanted)
                .with_context(|| {
                    format!(
                        "No Phonon voice is named {voice}. Choose from: {}.",
                        PHONON_VOICES.map(slug).join(", ")
                    )
                })?;
            return Ok((self, Some((name.to_owned(), "English"))));
        }
        if self.id().starts_with("kokoro:") {
            let name = KOKORO_VOICES
                .into_iter()
                .find(|name| name.eq_ignore_ascii_case(voice))
                .with_context(|| {
                    format!(
                        "No Kokoro voice is named {voice}. Choose from: {}.",
                        KOKORO_VOICES.join(", ")
                    )
                })?;
            let (_, speaks, parakeet) = KOKORO_LANGUAGES
                .into_iter()
                .find(|(code, ..)| name.starts_with(*code))
                .expect("every Kokoro voice names its language");
            // The Linux runner has no Whisper.
            ensure!(
                parakeet || cfg!(not(target_os = "linux")),
                "{name} speaks {speaks}, which Boubou hears only on a Mac for now."
            );
            return self.hearing((name.to_owned(), speaks), parakeet, "kokoro:82m");
        }
        if self.id().starts_with("kitten-tts:") {
            // A 0.8 checkpoint names its voices; earlier ones know only the IDs.
            let named = self.id().contains("-0.8");
            let pick = |(name, id): (&'static str, &'static str)| if named { name } else { id };
            let names: Vec<_> = KITTEN_VOICES.map(pick).map(str::to_lowercase).into();
            let name = KITTEN_VOICES
                .into_iter()
                .find(|(name, id)| {
                    name.eq_ignore_ascii_case(voice) || id.eq_ignore_ascii_case(voice)
                })
                .map(pick)
                .with_context(|| {
                    format!(
                        "No KittenTTS voice is named {voice}. Choose from: {}.",
                        names.join(", ")
                    )
                })?;
            return Ok((self, Some((name.to_owned(), "English"))));
        }
        let voices: Vec<Voice> = VOICES
            .iter()
            .map(|&(name, speaks)| (name.to_owned(), speaks))
            .chain(crate::modular::clones(cache))
            .collect();
        let names: Vec<_> = voices.iter().map(|(name, _)| name.to_lowercase()).collect();
        let names = names.join(", ");
        ensure!(
            self.id().starts_with("qwen3-tts:"),
            "{} has one voice. Qwen3-TTS models take --voice: {names}. So do Kokoro, KittenTTS, Supertonic, Pocket TTS and Phonon models.",
            self.id()
        );
        let (name, own) = voices
            .into_iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(voice))
            .with_context(|| format!("No voice is named {voice}. Choose from: {names}."))?;
        let speaker = (
            name,
            named(&crate::modular::LANGUAGES[..10])?.unwrap_or(own),
        );
        let parakeet = speaker.1 == "English";
        self.hearing(
            speaker,
            parakeet,
            "qwen3-tts:0.6b-customvoice or qwen3-tts:1.7b-customvoice",
        )
    }

    /// This model with `speaker`, or its Whisper entry when `parakeet` does not hear the
    /// speaker's language. `entries` names the models that have a Whisper entry.
    fn hearing(
        self,
        speaker: Voice,
        parakeet: bool,
        entries: &str,
    ) -> Result<(Self, Option<Voice>)> {
        if parakeet || crate::modular::whisper(self.id()) {
            return Ok((self, Some(speaker)));
        }
        let hears = MODULAR_IDS
            .iter()
            .find(|id| id.strip_suffix("-whisper") == Some(self.id()))
            .with_context(|| {
                format!(
                    "{} speaks {}. Use {entries}, which can hear it.",
                    speaker.0, speaker.1
                )
            })?;
        Ok((Self::Modular(hears), Some(speaker)))
    }
}

pub fn value_parser() -> impl TypedValueParser<Value = Model> {
    PossibleValuesParser::new(Model::all().map(Model::id))
        .map(|id| Model::find(&id).expect("validated model ID"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn profiles_select_the_engine_built_for_this_host() {
        let host = (std::env::consts::OS, std::env::consts::ARCH);
        for profile in PROFILES {
            assert_eq!(profile.runtime().unwrap().host, host);
        }
        let unsupported = Profile {
            runtimes: &[],
            ..DEFAULT
        };
        assert!(
            unsupported
                .runtime()
                .unwrap_err()
                .to_string()
                .contains("Apple Silicon Macs and x86-64 or arm64 Linux")
        );
    }

    #[test]
    fn a_voice_names_its_speaker_and_brings_the_ear_for_its_language() {
        let cache = std::env::temp_dir().join(format!("boubou-voiced-{}", std::process::id()));
        let spoken = |id, voice, language| {
            Model::Modular(id)
                .voiced(voice, language, &cache)
                .map(|(model, voice)| {
                    let (name, language) = voice.unwrap();
                    (model.id(), (name, language))
                })
                .map_err(|error| error.to_string())
        };
        let voiced = |id, voice| spoken(id, Some(voice), None);
        let voice = |name: &str, language| Ok((name.to_owned(), language));
        let small = "qwen3-tts:0.6b-customvoice";
        assert_eq!(
            voiced(small, "RYAN"),
            voice("Ryan", "English").map(|v| (small, v))
        );
        let whisper = "qwen3-tts:0.6b-customvoice-whisper";
        for id in [small, whisper] {
            assert_eq!(
                voiced(id, "ono_anna"),
                voice("Ono_Anna", "Japanese").map(|v| (whisper, v))
            );
        }
        assert_eq!(
            voiced(whisper, "aiden"),
            voice("Aiden", "English").map(|v| (whisper, v))
        );
        // A voice cloned on this computer is found by its name, and heard in its language.
        for (name, language) in [("mom", "zh"), ("dad", "en")] {
            crate::library::keep(&cache.join("voices"), name, language);
        }
        assert_eq!(
            voiced(small, "Mom"),
            voice("mom", "Chinese").map(|v| (whisper, v))
        );
        assert_eq!(
            voiced(small, "dad"),
            voice("dad", "English").map(|v| (small, v))
        );
        // A voice cloned in a language VoxCPM2 reads is heard by the Whisper entry.
        crate::library::keep(&cache.join("voices"), "nok", "th");
        assert_eq!(
            voiced(small, "nok"),
            voice("nok", "Thai").map(|v| (whisper, v))
        );
        fs::remove_file(cache.join("voices/nok.json")).unwrap();
        assert!(
            voiced(small, "nobody")
                .unwrap_err()
                .ends_with("eric, dad, mom.")
        );
        // --language names another of Qwen3-TTS's ten languages, by its code or its name. Any
        // but English brings the Whisper entry, with the speaker the entry starts with or the
        // one --voice names.
        let large = "qwen3-tts:1.7b-customvoice";
        let large_whisper = "qwen3-tts:1.7b-customvoice-whisper";
        for (id, name, language, expected) in [
            (small, Some("ryan"), "de", (whisper, ("Ryan", "German"))),
            (small, Some("mom"), "FR", (whisper, ("mom", "French"))),
            (small, None, "Russian", (whisper, ("Aiden", "Russian"))),
            (whisper, None, "pt", (whisper, ("Vivian", "Portuguese"))),
            (
                whisper,
                Some("sohee"),
                "es",
                (whisper, ("Sohee", "Spanish")),
            ),
            (
                small,
                Some("ono_anna"),
                "it",
                (whisper, ("Ono_Anna", "Italian")),
            ),
            (small, Some("vivian"), "en", (small, ("Vivian", "English"))),
            (
                large,
                Some("eric"),
                "es",
                (large_whisper, ("Eric", "Spanish")),
            ),
        ] {
            let (id_, (name_, language_)) = expected;
            assert_eq!(
                spoken(id, name, Some(language)),
                voice(name_, language_).map(|v| (id_, v))
            );
        }
        for language in ["nl", "hi", "klingon"] {
            let error = spoken(small, Some("ryan"), Some(language)).unwrap_err();
            let choices = "Choose from: en, ja, zh, ko, es, fr, de, pt, it, ru.";
            assert_eq!(
                error,
                format!("qwen3-tts does not speak {language}. {choices}")
            );
        }
        let error = spoken("qwen3-tts:1.7b-customvoice-9b", None, Some("de")).unwrap_err();
        assert!(error.starts_with("Aiden speaks German."), "{error}");
        fs::remove_dir_all(&cache).unwrap();
        // A Supertonic voice speaks any language Parakeet hears, English unless one is named.
        for (name, language, expected) in [
            (Some("f1"), None, ("F1", "English")),
            (Some("M5"), Some("fr"), ("M5", "French")),
            (None, Some("ukrainian"), ("M1", "Ukrainian")),
            (Some("f3"), Some("EL"), ("F3", "Greek")),
        ] {
            assert_eq!(
                spoken("supertonic:3", name, language),
                voice(expected.0, expected.1).map(|v| ("supertonic:3", v))
            );
        }
        for name in SUPERTONIC_VOICES {
            for (code, _) in SUPERTONIC_LANGUAGES {
                assert!(spoken("supertonic:3", Some(name), Some(code)).is_ok());
            }
        }
        let error = voiced("supertonic:3", "ryan").unwrap_err();
        assert!(
            error.starts_with("No Supertonic voice is named ryan.") && error.ends_with("f4, f5."),
            "{error}"
        );
        let error = spoken("supertonic:3", Some("f1"), Some("ja")).unwrap_err();
        assert!(
            error.starts_with("supertonic does not speak ja. Choose from: en, bg, hr,"),
            "{error}"
        );
        // A model whose voices speak one language each takes no --language.
        for (id, name) in [("kokoro:82m", Some("ef_dora")), ("phonon:31m-en", None)] {
            let error = spoken(id, name, Some("es")).unwrap_err();
            assert!(
                error.starts_with(&format!("{id} speaks one language")),
                "{error}"
            );
        }
        // A model size without a Whisper entry cannot hear the speaker's language.
        let error = voiced("qwen3-tts:1.7b-customvoice-9b", "sohee").unwrap_err();
        assert!(error.starts_with("Sohee speaks Korean."), "{error}");
        assert!(voiced(small, "nobody").unwrap_err().contains("ono_anna"));
        assert!(
            voiced("mms-tts:eng", "ryan")
                .unwrap_err()
                .starts_with("mms-tts:eng has one voice.")
        );
        // A Pocket TTS preset speaks the language of the profile's config.
        for (id, name, expected) in [
            (
                "pocket-tts:english_2026-10",
                "Marius",
                ("marius", "English"),
            ),
            ("pocket-tts:french_24l", "LOLA", ("lola", "French")),
            (
                "pocket-tts:english_2m_refl_2026-10",
                "bill_boerst",
                ("bill_boerst", "English"),
            ),
            ("pocket-tts:portuguese", "alba", ("alba", "Portuguese")),
        ] {
            assert_eq!(
                voiced(id, name),
                voice(expected.0, expected.1).map(|v| (id, v))
            );
        }
        for id in MODULAR_IDS
            .iter()
            .filter(|id| id.starts_with("pocket-tts:"))
        {
            assert!(pocket(id).is_some(), "{id}");
        }
        let error = voiced("pocket-tts:german", "ryan").unwrap_err();
        assert!(
            error.starts_with("No Pocket TTS voice is named ryan. Choose from: alba, marius,")
                && error.ends_with("daan, estelle."),
            "{error}"
        );
        // A Phonon voice goes by its catalog name, or with dashes for its spaces.
        for (name, expected) in [
            ("zoey", "Zoey"),
            ("Old Mortlake", "Old Mortlake"),
            ("captain-silas-blackwater", "Captain Silas Blackwater"),
            ("elodie-rose", "Elodie-Rose"),
        ] {
            assert_eq!(
                voiced("phonon:31m-en", name),
                voice(expected, "English").map(|v| ("phonon:31m-en", v))
            );
        }
        let error = voiced("phonon:31m-en", "ryan").unwrap_err();
        assert!(
            error.starts_with("No Phonon voice is named ryan. Choose from: zoey, juniper,")
                && error.ends_with("sir-bramwell, fairy-godmother."),
            "{error}"
        );
        // A Kokoro voice speaks the language its first letter names, and Parakeet hears it.
        for (id, name, language) in [
            ("kokoro:82m", "AF_BELLA", ("af_bella", "English")),
            ("kokoro:82m", "bm_george", ("bm_george", "English")),
            ("kokoro:82m-27b", "ef_dora", ("ef_dora", "Spanish")),
            ("kokoro:82m", "ff_siwis", ("ff_siwis", "French")),
            ("kokoro:82m", "im_nicola", ("im_nicola", "Italian")),
            ("kokoro:82m-9b", "pf_dora", ("pf_dora", "Portuguese")),
        ] {
            assert_eq!(
                voiced(id, name),
                voice(language.0, language.1).map(|v| (id, v))
            );
        }
        // Boubou downloads each voice it offers, and each speaks a language Kokoro has.
        let lock = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/qwen/lock_models.py"),
        )
        .unwrap();
        let locked = |list: &str| {
            let (_, locked) = lock.split_once(list).unwrap();
            let (locked, _) = locked.split_once(']').unwrap();
            locked.split('"').skip(1).step_by(2).collect::<Vec<_>>()
        };
        assert_eq!(locked("KOKORO_VOICES = ["), KOKORO_VOICES);
        assert_eq!(locked("SUPERTONIC_VOICES = ["), SUPERTONIC_VOICES);
        assert_eq!(locked("POCKET_PRESETS = ["), POCKET_VOICES);
        let phonon: Vec<_> = locked("PHONON_VOICES = [").into_iter().step_by(2).collect();
        assert_eq!(phonon, PHONON_VOICES);
        // Each Pocket TTS profile starts with the voice the lock gives its language.
        let (_, starts) = lock.split_once("POCKET_VOICES = {").unwrap();
        let (starts, _) = starts.split_once('}').unwrap();
        let starts: Vec<_> = starts.split('"').skip(1).step_by(2).collect();
        let languages: Vec<_> = POCKET_LANGUAGES
            .iter()
            .flat_map(|(word, _, voice)| [*word, *voice])
            .collect();
        assert_eq!(starts, languages);
        for name in KOKORO_VOICES {
            let hears = voiced("kokoro:82m", name);
            assert!(
                hears.is_ok() || cfg!(target_os = "linux") && "hjz".contains(&name[..1]),
                "{name}"
            );
        }
        // A KittenTTS voice goes by its name on 0.8 checkpoints, and by its ID on earlier ones.
        for (id, name, expected) in [
            ("kitten-tts:mini-0.8", "LUNA", "Luna"),
            ("kitten-tts:nano-0.8-int8", "expr-voice-5-m", "Leo"),
            ("kitten-tts:nano-0.2", "kiki", "expr-voice-5-f"),
            ("kitten-tts:mini-0.1", "expr-voice-2-m", "expr-voice-2-m"),
        ] {
            assert_eq!(
                voiced(id, name),
                voice(expected, "English").map(|v| (id, v))
            );
        }
        let error = voiced("kitten-tts:micro-0.8", "alba").unwrap_err();
        assert!(
            error.starts_with("No KittenTTS voice is named alba.") && error.ends_with("kiki, leo."),
            "{error}"
        );
        let error = voiced("kitten-tts:nano-0.1", "alba").unwrap_err();
        assert!(error.ends_with("expr-voice-5-m."), "{error}");
        // A Kokoro voice of a language Parakeet does not hear brings the Whisper entry, on a Mac.
        for (name, language) in [
            ("hf_alpha", "Hindi"),
            ("jm_kumo", "Japanese"),
            ("zf_xiaoxiao", "Chinese"),
        ] {
            let whisper = "kokoro:82m-whisper";
            if cfg!(target_os = "linux") {
                let error = voiced("kokoro:82m", name).unwrap_err();
                assert!(error.ends_with("only on a Mac for now."), "{error}");
                continue;
            }
            for id in ["kokoro:82m", whisper] {
                assert_eq!(
                    voiced(id, name),
                    voice(name, language).map(|v| (whisper, v))
                );
            }
            let error = voiced("kokoro:82m-9b", name).unwrap_err();
            assert_eq!(
                error,
                format!("{name} speaks {language}. Use kokoro:82m, which can hear it.")
            );
        }
        assert_eq!(
            voiced("kokoro:82m-whisper", "ef_dora"),
            voice("ef_dora", "Spanish").map(|v| ("kokoro:82m-whisper", v))
        );
        for missing in ["ryan", "jx_alpha", "dad"] {
            let error = voiced("kokoro:82m", missing).unwrap_err();
            assert!(
                error.starts_with(&format!("No Kokoro voice is named {missing}."))
                    && error.ends_with("zm_yunxia, zm_yunyang."),
                "{error}"
            );
        }
        let (model, voice) = Model::Liquid(&DEFAULT).voiced(None, None, &cache).unwrap();
        assert!(model.id() == DEFAULT.id && voice.is_none());
        for (voice, language) in [(Some("ryan"), None), (None, Some("fr"))] {
            let liquid = Model::Liquid(&DEFAULT);
            assert!(liquid.voiced(voice, language, &cache).is_err());
        }
    }
}
