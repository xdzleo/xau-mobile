//! Which eight games the farm broadcasts: one of three pools of
//! Microsoft-published games, switched with "Trocar jogos".
//!
//! Pool 1 is the core's `FIXED_CONSOLE_TITLES` (checked live with a
//! heartbeat read-back). Pools 2 and 3 were taken from Microsoft's own
//! catalogue (`displaycatalog` `AlternateIds.XboxTitleId`) or from the
//! accounts' title history, and every ID was confirmed on titlehub
//! (`/titles/batch/decoration/detail`) to resolve to that game with a
//! Microsoft publisher (Xbox Game Studios, Bethesda, Activision). None is
//! typed from memory. The chosen pool is saved, survives a restart and
//! applies to every account's next farm run.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use xau_core::error::{Error, Result};
use xau_core::xbox::farm::{FARM_MAX_TITLES, FIXED_CONSOLE_TITLES};

/// One game in the farm's set.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FarmTitle {
    pub id: String,
    pub name: String,
}

type Pool = [(&'static str, &'static str); FARM_MAX_TITLES];

/// Pools 2 and 3; pool 1 is `FIXED_CONSOLE_TITLES`.
const EXTRA_POOLS: [Pool; 2] = [
    [
        ("552499398", "Gears of War 4"),
        ("1829869520", "Gears of War: Reloaded"),
        ("1835926818", "Hellblade II: Senua's Saga"),
        ("1745314691", "Psychonauts 2"),
        ("1659804324", "Ori and the Will of the Wisps"),
        ("2105342780", "Grounded"),
        ("2100880776", "Pentiment"),
        ("1938834516", "State of Decay 2"),
    ],
    [
        ("1256782258", "Fallout 4"),
        ("1902585111", "DOOM: The Dark Ages"),
        ("1690468494", "DEATHLOOP"),
        ("1877522801", "Age of Mythology: Retold"),
        ("1879711255", "The Outer Worlds 2"),
        ("1985701699", "Forza Horizon 4"),
        ("219630713", "Halo 5: Guardians"),
        ("2046562785", "Avowed"),
    ],
];

/// How many pools there are.
pub const POOLS: usize = 1 + EXTRA_POOLS.len();

fn pool(index: usize) -> Vec<FarmTitle> {
    match index.checked_sub(1).and_then(|i| EXTRA_POOLS.get(i)) {
        Some(p) => p
            .iter()
            .map(|(id, name)| FarmTitle { id: (*id).into(), name: (*name).into() })
            .collect(),
        None => FIXED_CONSOLE_TITLES
            .iter()
            .map(|(id, name, _)| FarmTitle { id: (*id).into(), name: (*name).into() })
            .collect(),
    }
}

#[derive(Serialize, Deserialize)]
struct Saved {
    pool: usize,
}

fn path() -> Result<PathBuf> {
    Ok(crate::accounts::data_dir()?.join("farm_pool.json"))
}

/// Which pool is in use, 0-based. A missing or unreadable file is pool 0.
#[must_use]
pub fn current_pool() -> usize {
    path()
        .ok()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<Saved>(&b).ok())
        .map(|s| s.pool)
        .filter(|i| *i < POOLS)
        .unwrap_or(0)
}

/// The eight games the farm broadcasts now.
#[must_use]
pub fn current() -> Vec<FarmTitle> {
    pool(current_pool())
}

/// Move to the next pool (1 → 2 → 3 → 1), save it and return it with
/// its 0-based index.
///
/// # Errors
/// The choice could not be saved.
pub fn next() -> Result<(usize, Vec<FarmTitle>)> {
    let index = (current_pool() + 1) % POOLS;
    let body = serde_json::to_vec(&Saved { pool: index })?;
    std::fs::write(path()?, body)
        .map_err(|e| Error::other(format!("não foi possível salvar os jogos do farm: {e}")))?;
    Ok((index, pool(index)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_pool_is_eight_real_looking_distinct_ids() {
        let mut all = HashSet::new();
        for i in 0..POOLS {
            let p = pool(i);
            assert_eq!(p.len(), FARM_MAX_TITLES, "pool {i}");
            for t in &p {
                assert!(
                    !t.id.is_empty() && t.id.bytes().all(|b| b.is_ascii_digit()) && t.id.parse::<u32>().is_ok(),
                    "{}: {:?}",
                    t.name,
                    t.id
                );
                assert!(all.insert(t.id.clone()), "{} appears in two pools", t.name);
            }
        }
    }

    #[test]
    fn an_unknown_index_is_the_first_pool() {
        assert_eq!(pool(99), pool(0));
    }
}
