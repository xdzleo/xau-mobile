//! Store search and filter benchmark, run on the desktop against the real
//! Microsoft Store endpoints:
//!
//! ```text
//! cargo run --release -p xau-mobile --example store_bench
//! ```
//!
//! `BASELINE` re-creates what the app did before the optimization (a new
//! HTTP client per request, pages one after another, filtering on the
//! phone's UI thread side), inline, so the comparison stays reproducible
//! after the library changed.

use std::time::{Duration, Instant};

use serde_json::Value;
use xau_mobile::store::{self, StoreProduct};

/// Few runs and a pause between scenarios: the Store answers a burst of
/// requests from one address with 403.
const RUNS: usize = 3;
const BREATHE: Duration = Duration::from_secs(4);

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn ms(d: Duration) -> String {
    format!("{:>7.1} ms", d.as_secs_f64() * 1000.0)
}

async fn bench<F, Fut, T>(name: &str, mut f: F) -> (Duration, T)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let mut times = Vec::new();
    let mut last = None;
    for _ in 0..RUNS {
        let t = Instant::now();
        last = Some(f().await);
        times.push(t.elapsed());
    }
    let best = *times.iter().min().unwrap();
    let med = median(times);
    println!("{name:<58} median {}   best {}", ms(med), ms(best));
    (med, last.unwrap())
}

// ---- BASELINE: the app before the optimization ------------------------

fn list_url(list: &str, page: u32) -> reqwest::Url {
    let mut url =
        reqwest::Url::parse("https://apps.microsoft.com/api/Reco/GetComputedProductsList").unwrap();
    url.query_pairs_mut()
        .append_pair("listName", list)
        .append_pair("pgNo", &page.to_string())
        .append_pair("noItems", "50")
        .append_pair("filteredCategories", "AllProducts")
        .append_pair("mediaType", "Games")
        .append_pair("gl", "BR")
        .append_pair("hl", "pt-br")
        .append_pair("exp", "0");
    url
}

/// One page the old way: a brand-new client (new TLS handshake) per call.
async fn baseline_page(list: &str, page: u32) -> Vec<StoreProduct> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(20)).build().unwrap();
    let body = client
        .get(list_url(list, page))
        .header("accept", "application/json")
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    // An empty / non-JSON body is the end of the list.
    match serde_json::from_slice::<Value>(&body) {
        Ok(v) => store::parse_page(&v).products,
        Err(_) => Vec::new(),
    }
}

/// The same rules the app's filter has: price in reais, `Incluído` = list price.
fn price_of(p: &StoreProduct) -> Option<f64> {
    fn money(t: &str) -> Option<f64> {
        let d: String = t.chars().filter(|c| c.is_ascii_digit() || *c == ',' || *c == '.').collect();
        if !d.chars().any(|c| c.is_ascii_digit()) {
            return None;
        }
        let n = if d.contains(',') { d.replace('.', "").replace(',', ".") } else { d };
        n.parse().ok()
    }
    let t = p.price.as_deref()?;
    if t.to_lowercase().contains("gratuit") {
        return Some(0.0);
    }
    money(t).or_else(|| p.was.as_deref().and_then(money))
}

/// Walk the paid list a page at a time until `want` games cost at least `min`.
async fn baseline_until(min: f64, want: usize) -> usize {
    let mut found = 0;
    for page in 1..=30 {
        let batch = baseline_page("TopPaid", page).await;
        if batch.is_empty() {
            break;
        }
        found += batch.iter().filter(|p| price_of(p).is_some_and(|x| x >= min)).count();
        if found >= want {
            break;
        }
    }
    found
}

#[tokio::main]
async fn main() {
    let which = std::env::args().nth(1).unwrap_or_else(|| "baseline".into());
    println!("== store benchmark ({which}), {RUNS} runs each ==\n");

    if which == "baseline" {
        bench("first screen: 1 trending page (new client)", || baseline_page("TopTrending", 1)).await;
        tokio::time::sleep(BREATHE).await;
        bench("8 pages in a row (new client each, sequential)", || async {
            let mut n = 0;
            for p in 1..=8 {
                n += baseline_page("TopTrending", p).await.len();
            }
            n
        })
        .await;
        tokio::time::sleep(BREATHE).await;
        let (_, found) = bench("filter: >= R$ 200, until 30 games found", || baseline_until(200.0, 30)).await;
        println!("   (found {found})");
        tokio::time::sleep(BREATHE).await;
        let (_, found) = bench("filter: >= R$ 500, until 30 games found / list ends", || baseline_until(500.0, 30)).await;
        println!("   (found {found})");
    }

    if which == "depth" {
        // How deep each list goes, and its dearest game, at a gentle pace.
        for list in ["TopPaid", "TopTrending", "TopGrossing"] {
            let (mut total, mut max, mut pages) = (0usize, 0f64, Vec::new());
            let mut over = [0usize; 3];
            for n in 1..=30u32 {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let page = match store::featured(list, Some(&n.to_string())).await {
                    Ok(p) => p,
                    Err(e) => {
                        pages.push(format!("E({e})"));
                        break;
                    }
                };
                pages.push(page.products.len().to_string());
                if page.products.is_empty() {
                    break;
                }
                total += page.products.len();
                for p in &page.products {
                    if let Some(x) = store::price_of(p) {
                        max = max.max(x);
                        for (i, t) in [200.0, 300.0, 500.0].iter().enumerate() {
                            if x >= *t {
                                over[i] += 1;
                            }
                        }
                    }
                }
            }
            println!(
                "{list:<12} pages [{}] total {total}  dearest R$ {max:.2}  >=200: {}  >=300: {}  >=500: {}",
                pages.join(","),
                over[0],
                over[1],
                over[2]
            );
        }
    }

    if which == "optimized" {
        let none = store::Filter::default();
        let paid = |min: f64| store::Filter { min: Some(min), ..store::Filter::default() };
        if let Err(e) = store::browse("", "TopTrending", None, &none, 36).await {
            println!("first call failed: {e}");
            return;
        }

        // Network, cache emptied before every run.
        bench("first screen: 1 trending page (shared client, cold cache)", || async {
            store::clear_cache();
            store::browse("", "TopTrending", None, &none, 36).await.map(|p| p.products.len()).unwrap_or(0)
        })
        .await;
        tokio::time::sleep(BREATHE).await;
        bench("8 pages (shared client, 3 at a time, cold cache)", || async {
            store::clear_cache();
            let page = |n: u32| async move {
                store::featured("TopTrending", Some(&n.to_string())).await.map(|p| p.products.len()).unwrap_or(0)
            };
            let mut n = 0;
            for chunk in [[1, 2, 3], [4, 5, 6]] {
                let (a, b, c) = tokio::join!(page(chunk[0]), page(chunk[1]), page(chunk[2]));
                n += a + b + c;
            }
            let (a, b) = tokio::join!(page(7), page(8));
            n + a + b
        })
        .await;
        tokio::time::sleep(BREATHE).await;
        let (_, n) = bench("filter: >= R$ 200, until 30 games found (cold cache)", || async {
            store::clear_cache();
            match store::browse("", "TopPaid", None, &paid(200.0), 30).await {
                Ok(p) => p.products.len(),
                Err(e) => {
                    println!("   error: {e}");
                    0
                }
            }
        })
        .await;
        println!("   (found {n})");
        tokio::time::sleep(BREATHE).await;
        let (_, n) = bench("filter: >= R$ 500, until 30 games found (cold cache)", || async {
            store::clear_cache();
            store::browse("", "TopPaid", None, &paid(500.0), 30).await.map(|p| p.products.len()).unwrap_or(0)
        })
        .await;
        println!("   (found {n})");

        // Cache warm: what the phone does when a filter changes or the
        // store is reopened within ten minutes.
        println!();
        bench("warm: reopen store (first screen)", || async {
            store::browse("", "TopTrending", None, &none, 36).await.map(|p| p.products.len()).unwrap_or(0)
        })
        .await;
        bench("warm: change filter to >= R$ 500 on cached pages", || async {
            store::browse("", "TopPaid", None, &paid(500.0), 30).await.map(|p| p.products.len()).unwrap_or(0)
        })
        .await;
        bench("warm: change filter to Microsoft + sem gratis", || async {
            let f = store::Filter { microsoft_only: true, hide_free: true, ..store::Filter::default() };
            store::browse("", "TopPaid", None, &f, 30).await.map(|p| p.products.len()).unwrap_or(0)
        })
        .await;
    }
}
