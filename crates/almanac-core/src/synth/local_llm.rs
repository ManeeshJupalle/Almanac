//! Local on-device synthesis backend: Qwen2.5-0.5B-Instruct (GGUF Q4_K_M)
//! via candle. Pure in-process CPU inference from local files — no network
//! I/O exists in this module. Greedy decoding for determinism.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use candle_core::quantized::gguf_file;
use candle_core::{Device, Tensor};
use candle_transformers::generation::{LogitsProcessor, Sampling};
use candle_transformers::models::quantized_qwen2::ModelWeights;
use chrono::Datelike;

use super::{
    briefing_from_order, build_digest, parse_order, Briefing, DayContext, SynthesisBackend,
};
use crate::extract::ExtractedItem;

const MAX_NEW_TOKENS: usize = 320;

pub struct LocalLlmBackend {
    /// candle's forward pass needs &mut (KV cache) — serialized by a mutex.
    model: Mutex<ModelWeights>,
    tokenizer: tokenizers::Tokenizer,
    eos_ids: Vec<u32>,
    backend_id: String,
}

impl LocalLlmBackend {
    /// Load from a directory containing model.gguf + tokenizer.json.
    pub fn load(model_dir: &Path) -> Result<Self> {
        let gguf_path = model_dir.join("model.gguf");
        let tokenizer = tokenizers::Tokenizer::from_file(model_dir.join("tokenizer.json"))
            .map_err(|e| anyhow!("loading tokenizer: {e}"))?;

        let mut file = std::fs::File::open(&gguf_path)
            .with_context(|| format!("opening {}", gguf_path.display()))?;
        let content = gguf_file::Content::read(&mut file)
            .with_context(|| format!("reading GGUF {}", gguf_path.display()))?;
        let model = ModelWeights::from_gguf(content, &mut file, &Device::Cpu)
            .context("building Qwen2 weights from GGUF")?;

        let eos_ids = ["<|im_end|>", "<|endoftext|>"]
            .iter()
            .filter_map(|t| tokenizer.token_to_id(t))
            .collect::<Vec<_>>();
        if eos_ids.is_empty() {
            bail!("tokenizer has neither <|im_end|> nor <|endoftext|>");
        }

        Ok(Self {
            model: Mutex::new(model),
            tokenizer,
            eos_ids,
            backend_id: "local-qwen2.5-0.5b-instruct-q4_k_m".to_string(),
        })
    }

    /// Greedy generation. Blocking CPU work (seconds); callers off the main
    /// thread should wrap in spawn_blocking (UI concern, Phase 5).
    fn generate(&self, prompt: &str) -> Result<String> {
        let encoding =
            self.tokenizer.encode(prompt, true).map_err(|e| anyhow!("tokenize: {e}"))?;
        let prompt_ids: Vec<u32> = encoding.get_ids().to_vec();
        if prompt_ids.is_empty() {
            bail!("empty prompt");
        }

        let mut model = self.model.lock().expect("model mutex poisoned");
        let mut sampler = LogitsProcessor::from_sampling(0, Sampling::ArgMax);

        // Prompt pass (index_pos == 0 resets the KV cache in candle's
        // quantized qwen2, making the backend reusable across calls).
        let input = Tensor::new(prompt_ids.as_slice(), &Device::Cpu)?.unsqueeze(0)?;
        let logits = model.forward(&input, 0)?;
        let logits = logits.squeeze(0)?;
        let mut next = sampler.sample(&logits)?;

        let mut generated: Vec<u32> = Vec::new();
        for step in 0..MAX_NEW_TOKENS {
            if self.eos_ids.contains(&next) {
                break;
            }
            generated.push(next);
            let input = Tensor::new(&[next], &Device::Cpu)?.unsqueeze(0)?;
            let logits = model.forward(&input, prompt_ids.len() + step)?;
            let logits = logits.squeeze(0)?;
            next = sampler.sample(&logits)?;
        }

        self.tokenizer.decode(&generated, true).map_err(|e| anyhow!("decode: {e}"))
    }

    fn chat_prompt(task: &str) -> String {
        format!(
            "<|im_start|>system\nYou are Almanac, a careful assistant that plans a user's day. \
             You only reference the numbered items you are given — never invent items.<|im_end|>\n\
             <|im_start|>user\n{task}<|im_end|>\n<|im_start|>assistant\n"
        )
    }

    fn task_text(items: &[ExtractedItem], ctx: DayContext, correction: Option<&str>) -> String {
        let digest = build_digest(items);
        let n = items.len();
        let example =
            (1..=n).map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
        let mut task = format!(
            "Today is {weekday}, {date}. These are today's items, numbered:\n{digest}\n\n\
             Order ALL {n} items into the best sequence for the day. Keep scheduled events in \
             time order; put urgent actions before loose commitments.\n\
             Reply with EXACTLY ONE line and nothing else, containing only numbers \
             (in your chosen order):\n\
             ORDER: {example}",
            weekday = ctx.date.weekday(),
            date = ctx.date,
        );
        if let Some(err) = correction {
            task.push_str(&format!(
                "\n\nYour previous reply was invalid ({err}). Follow the format exactly."
            ));
        }
        task
    }
}

#[async_trait]
impl SynthesisBackend for LocalLlmBackend {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    async fn synthesize(&self, items: Vec<ExtractedItem>, ctx: DayContext) -> Result<Briefing> {
        let mut correction: Option<String> = None;
        for _attempt in 0..2 {
            let prompt = Self::chat_prompt(&Self::task_text(&items, ctx, correction.as_deref()));
            let reply = self.generate(&prompt)?;
            match parse_order(&reply, items.len()) {
                Ok(order) => return briefing_from_order(&items, order),
                Err(err) => correction = Some(format!("{err:#}")),
            }
        }
        bail!(
            "local model failed to produce a valid plan after a retry: {}",
            correction.unwrap_or_default()
        )
    }
}
