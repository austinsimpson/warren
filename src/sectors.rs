//! Sectors for stocks, from SEC industry (SIC) codes via EDGAR, cached in
//! `<data dir>/warren/sectors.json`.
//!
//! SIC codes predate GICS sectors, so the mapping is approximate. Notably,
//! SIC 7370–7379 (software and data processing) holds internet and media
//! companies that GICS puts in communication services; they land in
//! technology here.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::market::edgar::EdgarClient;
use crate::strategy::{Sector, Universe};

/// Company records are refetched after this long.
const COMPANY_TTL_DAYS: i64 = 90;
/// The ticker list is refetched after this long, to catch new listings.
const TICKERS_TTL_DAYS: i64 = 7;

/// The sector for an SEC industry code, or `None` for codes that don't
/// identify one (e.g. 9995, non-operating establishments).
pub fn sector_for_sic(sic: u32) -> Option<Sector> {
    use Sector::*;
    Some(match sic {
        100..=999 => ConsumerStaples,
        1000..=1099 => Materials,
        1200..=1399 => Energy,
        1400..=1499 => Materials,
        1520..=1531 => ConsumerDiscretionary, // homebuilders
        1500..=1799 => Industrials,
        2000..=2199 => ConsumerStaples,
        2200..=2399 => ConsumerDiscretionary,
        2400..=2499 => Materials,
        2500..=2599 => ConsumerDiscretionary,
        2600..=2699 => Materials,
        2700..=2799 => Communication,
        2830..=2836 => HealthCare,
        2840..=2844 => ConsumerStaples,
        2800..=2899 => Materials,
        2900..=2999 => Energy,
        3010..=3011 => ConsumerDiscretionary, // tires
        3000..=3099 => Materials,
        3100..=3199 => ConsumerDiscretionary,
        3200..=3399 => Materials,
        3400..=3499 => Industrials,
        3570..=3579 => Technology,
        3500..=3599 => Industrials,
        3630..=3639 | 3650..=3652 => ConsumerDiscretionary,
        3660..=3699 => Technology,
        3600..=3629 | 3640..=3649 => Industrials,
        3711..=3716 | 3751 => ConsumerDiscretionary,
        3700..=3799 => Industrials,
        3840..=3851 => HealthCare,
        3873 => ConsumerDiscretionary,
        3800..=3899 => Technology,
        3900..=3999 => ConsumerDiscretionary,
        4600..=4619 => Energy, // pipelines
        4000..=4799 => Industrials,
        4800..=4899 => Communication,
        4950..=4959 => Industrials, // waste management
        4900..=4999 => Utilities,
        5120..=5129 => HealthCare,
        5140..=5149 => ConsumerStaples,
        5170..=5179 => Energy,
        5000..=5199 => Industrials,
        5331 | 5399 | 5400..=5499 | 5912 => ConsumerStaples,
        5200..=5999 => ConsumerDiscretionary,
        6500..=6553 | 6798 => RealEstate,
        6000..=6799 => Financials,
        7000..=7099 | 7200..=7299 => ConsumerDiscretionary,
        7310..=7319 => Communication,
        7370..=7379 => Technology,
        7300..=7399 => Industrials,
        7500..=7599 => ConsumerDiscretionary,
        7800..=7899 => Communication,
        7900..=7999 => ConsumerDiscretionary,
        8000..=8099 | 8731 => HealthCare,
        8200..=8299 => ConsumerDiscretionary,
        8700..=8799 => Industrials,
        _ => return None,
    })
}

/// What Warren knows about a symbol's company.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompanyProfile {
    /// `None` when the SEC has no record of the ticker (e.g. some ADRs and
    /// funds) or its industry code doesn't map to a sector.
    pub sector: Option<Sector>,
    pub sic_description: String,
    pub foreign: bool,
    pub fetched_at: DateTime<Utc>,
}

impl CompanyProfile {
    /// Why `universe` excludes this company, if it does.
    pub fn exclusion(&self, universe: &Universe) -> Option<String> {
        if !universe.sectors.is_empty() {
            match self.sector {
                None => {
                    return Some(
                        "its sector is unknown and this agent only trades chosen sectors".into(),
                    );
                }
                Some(sector) if !universe.sectors.contains(&sector) => {
                    return Some(format!(
                        "it is in {}, outside this agent's sectors",
                        sector.label()
                    ));
                }
                _ => {}
            }
        }
        if self.foreign && !universe.allow_foreign {
            return Some("it is incorporated outside the US and this agent is US-only".into());
        }
        None
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    tickers: HashMap<String, u64>,
    tickers_fetched_at: Option<DateTime<Utc>>,
    profiles: HashMap<String, CompanyProfile>,
}

/// Company profiles by symbol, fetched from EDGAR on demand and cached.
pub struct SectorDirectory {
    edgar: EdgarClient,
    path: PathBuf,
    cache: Mutex<Cache>,
}

impl SectorDirectory {
    pub fn open(edgar: EdgarClient, data_root: &Path) -> Self {
        let path = data_root.join("sectors.json");
        let cache = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            edgar,
            path,
            cache: Mutex::new(cache),
        }
    }

    fn save(&self) -> Result<()> {
        let bytes = serde_json::to_vec(&*self.cache.lock().unwrap())?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &self.path).with_context(|| format!("writing {}", self.path.display()))
    }

    /// Profiles for `symbols`, fetching what isn't cached or is stale.
    /// Symbols that fail to fetch are left out.
    pub async fn profiles(&self, symbols: &[String]) -> Result<HashMap<String, CompanyProfile>> {
        let now = Utc::now();
        let tickers_stale = self
            .cache
            .lock()
            .unwrap()
            .tickers_fetched_at
            .is_none_or(|at| now - at > Duration::days(TICKERS_TTL_DAYS));
        if tickers_stale {
            let tickers = self.edgar.tickers().await?;
            let mut cache = self.cache.lock().unwrap();
            cache.tickers = tickers;
            cache.tickers_fetched_at = Some(now);
        }

        let mut out = HashMap::new();
        let mut fetched = false;
        for symbol in symbols {
            let (cached, cik) = {
                let cache = self.cache.lock().unwrap();
                (
                    cache.profiles.get(symbol).cloned(),
                    cache.tickers.get(symbol).copied(),
                )
            };
            if let Some(profile) =
                cached.filter(|p| now - p.fetched_at < Duration::days(COMPANY_TTL_DAYS))
            {
                out.insert(symbol.clone(), profile);
                continue;
            }
            let profile = match cik {
                Some(cik) => match self.edgar.company(cik).await {
                    Ok(info) => CompanyProfile {
                        sector: info.sic.and_then(sector_for_sic),
                        sic_description: info.sic_description.clone(),
                        foreign: info.is_foreign(),
                        fetched_at: now,
                    },
                    Err(_) => continue,
                },
                // Not an SEC filer under this ticker.
                None => CompanyProfile {
                    sector: None,
                    sic_description: String::new(),
                    foreign: false,
                    fetched_at: now,
                },
            };
            self.cache
                .lock()
                .unwrap()
                .profiles
                .insert(symbol.clone(), profile.clone());
            out.insert(symbol.clone(), profile);
            fetched = true;
        }
        if fetched || tickers_stale {
            self.save()?;
        }
        Ok(out)
    }

    pub async fn profile(&self, symbol: &str) -> Result<Option<CompanyProfile>> {
        Ok(self.profiles(&[symbol.to_string()]).await?.remove(symbol))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Looks up a few companies on EDGAR; needs edgar.contact. Run with
    /// `cargo test live_sectors -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_sectors() {
        let config = crate::config::Config::load_or_create().unwrap();
        let dir = std::env::temp_dir().join(format!("warren-sectors-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let directory =
            SectorDirectory::open(EdgarClient::new(&config.edgar.contact).unwrap(), &dir);
        let symbols: Vec<String> = ["AAPL", "NU", "ITUB", "SPCX", "BRK.B", "XOM", "SPY", "VZ"]
            .map(String::from)
            .to_vec();
        let profiles = crate::runtime::RUNTIME
            .block_on(directory.profiles(&symbols))
            .unwrap();
        for symbol in &symbols {
            println!("{symbol:6} {:?}", profiles.get(symbol));
        }
        assert_eq!(profiles["AAPL"].sector, Some(Sector::Technology));
        assert!(profiles["NU"].foreign);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn maps_familiar_companies() {
        assert_eq!(sector_for_sic(3571), Some(Sector::Technology)); // Apple
        assert_eq!(sector_for_sic(3674), Some(Sector::Technology)); // semiconductors
        assert_eq!(sector_for_sic(2834), Some(Sector::HealthCare)); // pharma
        assert_eq!(sector_for_sic(6022), Some(Sector::Financials)); // banks
        assert_eq!(sector_for_sic(6798), Some(Sector::RealEstate)); // REITs
        assert_eq!(sector_for_sic(1311), Some(Sector::Energy)); // oil & gas
        assert_eq!(sector_for_sic(4911), Some(Sector::Utilities));
        assert_eq!(sector_for_sic(3711), Some(Sector::ConsumerDiscretionary)); // autos
        assert_eq!(sector_for_sic(5331), Some(Sector::ConsumerStaples)); // variety stores
        assert_eq!(sector_for_sic(4813), Some(Sector::Communication)); // telecom
        assert_eq!(sector_for_sic(9995), None);
    }

    #[test]
    fn exclusion_respects_sectors_and_foreign() {
        let profile = CompanyProfile {
            sector: Some(Sector::Financials),
            sic_description: String::new(),
            foreign: true,
            fetched_at: Utc::now(),
        };
        let mut universe = Universe::default();
        assert!(profile.exclusion(&universe).is_none());
        universe.sectors = vec![Sector::Technology];
        assert!(profile.exclusion(&universe).unwrap().contains("Financials"));
        universe.sectors = vec![Sector::Financials];
        universe.allow_foreign = false;
        assert!(profile.exclusion(&universe).unwrap().contains("US-only"));
        let unknown = CompanyProfile {
            sector: None,
            ..profile
        };
        universe.allow_foreign = true;
        assert!(unknown.exclusion(&universe).unwrap().contains("unknown"));
    }
}
