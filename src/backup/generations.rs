//! Restore moves over any vector store: load the next generation, check it, then point the alias at it.
//! The previous generation stays as the rollback.

use crate::config::VectorConfig;
use crate::error::Result;
use crate::vector::VectorStore;
pub use crate::vector::qdrant::AliasState;
use crate::vector::qdrant::QdrantVectors;
use std::path::Path;
use std::sync::Arc;

#[async_trait::async_trait]
pub trait Generations: Send + Sync {
    async fn state(&self, alias: &str) -> Result<AliasState>;
    async fn next(&self, alias: &str) -> Result<String>;
    /// `target` must not exist.
    async fn import(&self, alias: &str, from: &Path, target: &str, dim: usize) -> Result<()>;
    /// For `--reembed`.
    async fn create_empty(&self, alias: &str, target: &str, dim: usize) -> Result<()>;
    async fn count(&self, alias: &str, target: &str) -> Result<u64>;
    /// `target` is not the one the alias serves.
    async fn discard(&self, alias: &str, target: &str) -> Result<()>;
    /// The previous generation is left in place.
    async fn adopt(&self, alias: &str, target: &str) -> Result<()>;
    async fn open(&self, alias: &str) -> Result<Arc<dyn VectorStore>>;
}

/// Creates no alias as a side effect, unlike `QdrantFactory::ensure_collection`.
pub struct QdrantGenerations {
    pub cfg: VectorConfig,
}

impl QdrantGenerations {
    async fn at(&self, alias: &str) -> Result<QdrantVectors> {
        QdrantVectors::connect(&VectorConfig {
            collection: alias.to_string(),
            ..self.cfg.clone()
        })
        .await
    }
}

#[async_trait::async_trait]
impl Generations for QdrantGenerations {
    async fn state(&self, alias: &str) -> Result<AliasState> {
        self.at(alias).await?.alias_state().await
    }

    async fn next(&self, alias: &str) -> Result<String> {
        self.at(alias).await?.next_generation_name().await
    }

    async fn import(&self, alias: &str, from: &Path, target: &str, dim: usize) -> Result<()> {
        self.at(alias)
            .await?
            .import_snapshot(from, target, dim)
            .await
    }

    async fn create_empty(&self, alias: &str, target: &str, dim: usize) -> Result<()> {
        self.at(alias)
            .await?
            .create_empty_generation(target, dim)
            .await
    }

    async fn count(&self, alias: &str, target: &str) -> Result<u64> {
        self.at(alias).await?.generation_count(target).await
    }

    async fn discard(&self, alias: &str, target: &str) -> Result<()> {
        self.at(alias).await?.drop_generation(target).await
    }

    async fn adopt(&self, alias: &str, target: &str) -> Result<()> {
        self.at(alias).await?.adopt_generation(target).await
    }

    async fn open(&self, alias: &str) -> Result<Arc<dyn VectorStore>> {
        Ok(Arc::new(self.at(alias).await?))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! In-memory generations, named the way Qdrant's are (`{alias}_v{n}`).

    use super::*;
    use crate::error::Error;
    use crate::vector::memory::MemoryVectors;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Mutex;

    #[derive(Default)]
    pub(crate) struct MemGens {
        pub aliases: Mutex<HashMap<String, String>>,
        pub gens: Mutex<BTreeMap<String, (Arc<MemoryVectors>, usize)>>,
    }

    fn number(alias: &str, name: &str) -> Option<u32> {
        name.strip_prefix(&format!("{alias}_v"))?.parse().ok()
    }

    impl MemGens {
        pub(crate) fn serve(&self, alias: &str, store: MemoryVectors, dim: usize) {
            let name = format!("{alias}_v1");
            self.gens
                .lock()
                .unwrap()
                .insert(name.clone(), (Arc::new(store), dim));
            self.aliases.lock().unwrap().insert(alias.into(), name);
        }

        pub(crate) fn served(&self, alias: &str) -> Option<String> {
            self.aliases.lock().unwrap().get(alias).cloned()
        }

        pub(crate) fn names(&self) -> Vec<String> {
            self.gens.lock().unwrap().keys().cloned().collect()
        }

        pub(crate) fn wipe(&self, alias: &str) {
            self.aliases.lock().unwrap().remove(alias);
            self.gens
                .lock()
                .unwrap()
                .retain(|n, _| number(alias, n).is_none());
        }

        fn store(&self, name: &str) -> Result<Arc<MemoryVectors>> {
            self.gens
                .lock()
                .unwrap()
                .get(name)
                .map(|(s, _)| s.clone())
                .ok_or_else(|| Error::Vector(format!("`{name}` does not exist")))
        }
    }

    #[async_trait::async_trait]
    impl Generations for MemGens {
        async fn state(&self, alias: &str) -> Result<AliasState> {
            Ok(match self.served(alias) {
                None => AliasState::Absent,
                Some(g) => match self.gens.lock().unwrap().get(&g) {
                    None => AliasState::Dangling(g),
                    Some((_, dim)) => AliasState::Live {
                        dim: *dim,
                        generation: g,
                    },
                },
            })
        }

        async fn next(&self, alias: &str) -> Result<String> {
            let n = self
                .gens
                .lock()
                .unwrap()
                .keys()
                .filter_map(|g| number(alias, g))
                .max()
                .unwrap_or(0);
            Ok(format!("{alias}_v{}", n + 1))
        }

        async fn import(&self, _alias: &str, from: &Path, target: &str, dim: usize) -> Result<()> {
            if self.gens.lock().unwrap().contains_key(target) {
                return Err(Error::Vector(format!("`{target}` already exists")));
            }
            let m = MemoryVectors::new();
            m.import_snapshot(from, target, dim).await?;
            self.gens
                .lock()
                .unwrap()
                .insert(target.into(), (Arc::new(m), dim));
            Ok(())
        }

        async fn create_empty(&self, _alias: &str, target: &str, dim: usize) -> Result<()> {
            self.gens
                .lock()
                .unwrap()
                .insert(target.into(), (Arc::new(MemoryVectors::new()), dim));
            Ok(())
        }

        async fn count(&self, _alias: &str, target: &str) -> Result<u64> {
            self.store(target)?.count().await
        }

        async fn discard(&self, _alias: &str, target: &str) -> Result<()> {
            self.gens.lock().unwrap().remove(target);
            Ok(())
        }

        async fn adopt(&self, alias: &str, target: &str) -> Result<()> {
            self.store(target)?;
            self.aliases
                .lock()
                .unwrap()
                .insert(alias.into(), target.into());
            Ok(())
        }

        async fn open(&self, alias: &str) -> Result<Arc<dyn VectorStore>> {
            let g = self
                .served(alias)
                .ok_or_else(|| Error::Vector(format!("alias `{alias}` does not exist")))?;
            Ok(self.store(&g)?)
        }
    }

    #[async_trait::async_trait]
    impl crate::tenants::VectorFactory for MemGens {
        async fn open(&self, alias: &str, _dim: usize) -> Result<Arc<dyn VectorStore>> {
            Generations::open(self, alias).await
        }
    }
}
