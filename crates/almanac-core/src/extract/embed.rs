//! all-MiniLM-L6-v2 sentence embeddings via tract-onnx (pure Rust, fully
//! on-device). The model and tokenizer are loaded from local files; there is
//! deliberately no code path here that can perform network I/O.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use tract_onnx::prelude::*;

const MAX_LEN: usize = 128;
const EMBED_DIM: usize = 384;

type RunnableOnnx = std::sync::Arc<TypedSimplePlan>;

pub struct EmbeddingModel {
    tokenizer: tokenizers::Tokenizer,
    plan: RunnableOnnx,
    /// Position of (input_ids, attention_mask, token_type_ids) in the
    /// model's input list — mapped by name, order-agnostic.
    input_positions: [usize; 3],
}

impl EmbeddingModel {
    pub fn load(model_dir: &Path) -> Result<Self> {
        let tokenizer_path = model_dir.join("tokenizer.json");
        let model_path = model_dir.join("model.onnx");
        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow!("loading {}: {e}", tokenizer_path.display()))?;

        let mut model = tract_onnx::onnx()
            .model_for_path(&model_path)
            .with_context(|| format!("loading {}", model_path.display()))?;

        // Map inputs by name so export order doesn't matter.
        let input_outlets = model.input_outlets()?.to_vec();
        let mut names = Vec::new();
        for outlet in &input_outlets {
            names.push(model.node(outlet.node).name.clone());
        }
        let position = |needle: &str| -> Result<usize> {
            names
                .iter()
                .position(|n| n == needle)
                .with_context(|| format!("model has no input named '{needle}' (inputs: {names:?})"))
        };
        let input_positions =
            [position("input_ids")?, position("attention_mask")?, position("token_type_ids")?];

        for i in 0..input_outlets.len() {
            model.set_input_fact(
                i,
                InferenceFact::dt_shape(i64::datum_type(), tvec!(1, MAX_LEN)),
            )?;
        }
        let plan = model
            .into_optimized()
            .context("optimizing MiniLM graph")?
            .into_runnable()
            .context("building runnable MiniLM plan")?;

        Ok(Self { tokenizer, plan, input_positions })
    }

    /// Embed one text into a 384-dim L2-normalized vector.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow!("tokenizing failed: {e}"))?;

        let mut ids = vec![0i64; MAX_LEN];
        let mut mask = vec![0i64; MAX_LEN];
        let mut type_ids = vec![0i64; MAX_LEN];
        let take = encoding.get_ids().len().min(MAX_LEN);
        for i in 0..take {
            ids[i] = encoding.get_ids()[i] as i64;
            mask[i] = encoding.get_attention_mask()[i] as i64;
            type_ids[i] = encoding.get_type_ids()[i] as i64;
        }

        let tensor = |data: &[i64]| -> Result<Tensor> {
            Ok(Tensor::from_shape(&[1, MAX_LEN], data)?)
        };
        let mut inputs: TVec<TValue> = tvec!(
            tensor(&ids)?.into(),
            tensor(&ids)?.into(),
            tensor(&ids)?.into()
        );
        inputs[self.input_positions[0]] = tensor(&ids)?.into();
        inputs[self.input_positions[1]] = tensor(&mask)?.into();
        inputs[self.input_positions[2]] = tensor(&type_ids)?.into();

        let outputs = self.plan.run(inputs)?;
        let hidden = outputs[0]
            .to_plain_array_view::<f32>()
            .context("model output was not f32")?;

        let pooled: Vec<f32> = match hidden.ndim() {
            // [1, seq, dim] token embeddings → attention-mask mean pooling
            3 => {
                let mut sums = vec![0f32; EMBED_DIM];
                let mut count = 0f32;
                for (i, &m) in mask.iter().enumerate().take(MAX_LEN) {
                    if m == 0 {
                        continue;
                    }
                    count += 1.0;
                    for d in 0..EMBED_DIM {
                        sums[d] += hidden[[0, i, d]];
                    }
                }
                if count == 0.0 {
                    bail!("cannot embed empty token sequence");
                }
                sums.iter().map(|s| s / count).collect()
            }
            // [1, dim] already pooled
            2 => (0..EMBED_DIM).map(|d| hidden[[0, d]]).collect(),
            n => bail!("unexpected model output rank {n}"),
        };

        // L2 normalize so cosine similarity is a plain dot product.
        let norm = pooled.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm == 0.0 {
            bail!("zero-norm embedding");
        }
        Ok(pooled.iter().map(|v| v / norm).collect())
    }
}

/// Dot product of two L2-normalized vectors == cosine similarity.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}
