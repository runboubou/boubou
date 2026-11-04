use clap::builder::{PossibleValuesParser, TypedValueParser};

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
    pub archive: Download<'static>,
    pub directory: &'static str,
    pub server: &'static str,
}

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
    pub runtime: Runtime,
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
    runtime: Runtime {
        label: "llama.cpp b11960",
        archive: Download {
            name: "llama-b11960-bin-macos-arm64.tar.gz",
            url: "https://github.com/ggml-org/llama.cpp/releases/download/b11960/llama-b11960-bin-macos-arm64.tar.gz",
            size: 16_465_074,
            sha256: "58e5a5e1d7c2bf66cedd9c29e0323f3e570660a70a1e2156d32eed08f4cc1e3c",
        },
        directory: "runtimes/b11960-macos-arm64",
        server: "llama-b11960/llama-server",
    },
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
    // Share one PCM16 SSE format; the older English archive emits float32 audio.
    runtime: Runtime {
        label: "Liquid Audio 8256 (22e8cd6f6)",
        archive: Download {
            name: "liquid-audio-jp-64b96718b341dbd5650f9e85627cecdcbd4ac61b-macos-arm64.zip",
            url: "https://huggingface.co/LiquidAI/LFM2.5-Audio-1.5B-JP-GGUF/resolve/64b96718b341dbd5650f9e85627cecdcbd4ac61b/runners/llama-liquid-audio-macos-arm64.zip",
            size: 7_280_738,
            sha256: "e59b7be254b6386960fc1d3cf51851709220202a4fdf7f83eb29953987992c9b",
        },
        directory: "runtimes/liquid-audio-jp-64b96718b341dbd5650f9e85627cecdcbd4ac61b-macos-arm64",
        server: "llama-liquid-audio-macos-arm64/llama-liquid-audio-server",
    },
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

pub static LFM25_JP: Profile = Profile {
    id: "LFM2.5-Audio-1.5B-JP",
    model_directory: "models/lfm2.5-audio-1.5b-jp/64b96718b341dbd5650f9e85627cecdcbd4ac61b",
    voice_chat_language: "Japanese",
    model: Download {
        name: "LFM2.5-Audio-1.5B-JP-Q4_0.gguf",
        url: "https://huggingface.co/LiquidAI/LFM2.5-Audio-1.5B-JP-GGUF/resolve/64b96718b341dbd5650f9e85627cecdcbd4ac61b/LFM2.5-Audio-1.5B-JP-Q4_0.gguf",
        size: 695_750_848,
        sha256: "d74e971956010953f5fbdf46ea8c4db6873b1b13a7893c4830f39f2ce7bba313",
    },
    projector: Download {
        name: "mmproj-LFM2.5-Audio-1.5B-JP-Q8_0.gguf",
        url: "https://huggingface.co/LiquidAI/LFM2.5-Audio-1.5B-JP-GGUF/resolve/64b96718b341dbd5650f9e85627cecdcbd4ac61b/mmproj-LFM2.5-Audio-1.5B-JP-Q8_0.gguf",
        size: 293_443_840,
        sha256: "710b339f336488b97800761b26a843c326fba6e390c532bcb6dcd5fa34dcf521",
    },
    license: Download {
        name: "LICENSE",
        url: "https://huggingface.co/LiquidAI/LFM2.5-Audio-1.5B-JP-GGUF/resolve/64b96718b341dbd5650f9e85627cecdcbd4ac61b/LICENSE",
        size: 10_575,
        sha256: "c138fe99cb339429d6daba8aa44c3dc5996e1a8caf55dec76170f8d2d626ddf7",
    },
    protocol: Protocol::LiquidAudio {
        tokenizer: Download {
            name: "tokenizer-LFM2.5-Audio-1.5B-JP-Q8_0.gguf",
            url: "https://huggingface.co/LiquidAI/LFM2.5-Audio-1.5B-JP-GGUF/resolve/64b96718b341dbd5650f9e85627cecdcbd4ac61b/tokenizer-LFM2.5-Audio-1.5B-JP-Q8_0.gguf",
            size: 74_584_224,
            sha256: "744a50aacb44a573b64bb33c5b764afb3b1dd88542f7f25dfd8272a7dcc52627",
        },
        vocoder: Download {
            name: "vocoder-LFM2.5-Audio-1.5B-JP-Q8_0.gguf",
            url: "https://huggingface.co/LiquidAI/LFM2.5-Audio-1.5B-JP-GGUF/resolve/64b96718b341dbd5650f9e85627cecdcbd4ac61b/vocoder-LFM2.5-Audio-1.5B-JP-Q8_0.gguf",
            size: 205_742_368,
            sha256: "e8b13a4b6964c3e7c12aa0c9b6d3a98ebc5128cca01292660cf7a893a5c3b7c6",
        },
    },
    ..LFM25
};

pub static PROFILES: &[&Profile] = &[&DEFAULT, &OMNI, &LFM25, &LFM25_JP];

pub fn value_parser() -> impl TypedValueParser<Value = &'static Profile> {
    PossibleValuesParser::new(PROFILES.iter().map(|profile| profile.id)).map(|id| {
        PROFILES
            .iter()
            .copied()
            .find(|profile| profile.id == id)
            .expect("the model ID was validated against the registry")
    })
}
