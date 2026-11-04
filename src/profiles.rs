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
pub struct Profile {
    pub id: &'static str,
    pub model_directory: &'static str,
    pub quantization: &'static str,
    pub voice_chat_language: &'static str,
    pub model: Download<'static>,
    pub projector: Download<'static>,
    pub license: Download<'static>,
    pub runtime: Runtime,
    /// Conservative runtime memory budget, including headroom; None excludes recommendations.
    pub memory_budget: Option<u64>,
    pub context_tokens: usize,
    pub max_output_tokens: usize,
    pub audio_samples_per_token: usize,
    pub instructions: &'static str,
    pub text_sampling: &'static [&'static str],
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
    text_sampling: &[
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

pub static PROFILES: &[&Profile] = &[&DEFAULT];

pub fn value_parser() -> impl TypedValueParser<Value = &'static Profile> {
    PossibleValuesParser::new(PROFILES.iter().map(|profile| profile.id)).map(|id| {
        PROFILES
            .iter()
            .copied()
            .find(|profile| profile.id == id)
            .expect("the model ID was validated against the registry")
    })
}
