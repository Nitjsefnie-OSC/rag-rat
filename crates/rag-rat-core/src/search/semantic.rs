//! Optional semantic search extension point.
//!
//! The current implementation ships a BM25 lexical floor and keeps embedding
//! storage in the schema. Embedding providers can implement this trait without
//! changing the CLI or MCP tool contract.

pub trait EmbeddingProvider {
    fn model_id(&self) -> &str;
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;
}
