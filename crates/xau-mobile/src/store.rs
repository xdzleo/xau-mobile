//! The Microsoft Store's game catalogue: search and what is trending.
//!
//! Same public endpoints the xodus-nexus launcher browses the Store
//! with, no sign-in needed:
//!
//! ```text
//! GET https://apps.microsoft.com/api/products/search
//!         ?query=…&mediaType=games&age=all&price=all&category=all
//!         &subscription=all&gl=BR&hl=pt-br[&cursor=…]
//! GET https://apps.microsoft.com/api/Reco/GetComputedProductsList
//!         ?listName=TopTrending&pgNo=1&noItems=24&filteredCategories=AllProducts
//!         &mediaType=Games&gl=BR&hl=pt-br&exp=0
//! ```
//!
//! Buying is not done here: it happens on the Store's own page for the
//! product (see [`purchase_url`]), in the app's WebView, where the user
//! confirms and pays exactly as on the web.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use xau_core::error::{Error, Result};

const MARKET: &str = "BR";
const LANG: &str = "pt-br";

/// One game as the store screen draws it.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct StoreProduct {
    /// Big id, upper case: `9NKX70BBCDRN`.
    pub product_id: String,
    pub title: String,
    pub publisher: Option<String>,
    /// Box art (title-branded), else poster, https.
    pub image: Option<String>,
    /// `R$ 249,00`, or `Gratuito`.
    pub price: Option<String>,
    /// The price before a discount, when there is one.
    pub was: Option<String>,
    /// `Xbox Game Pass` when a pass includes it.
    pub pass: Option<String>,
    /// 0-5.
    pub rating: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct StorePage {
    pub products: Vec<StoreProduct>,
    /// Pass back to get the next page; absent on the last one.
    pub cursor: Option<String>,
}

/// One client for every Store call: its connections stay open between
/// calls, so a page costs one round trip, not a new TLS handshake.
fn http() -> Result<&'static reqwest::Client> {
    if let Some(c) = CLIENT.get() {
        return Ok(c);
    }
    let c = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(8)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(30))
        .build()?;
    Ok(CLIENT.get_or_init(|| c))
}

static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// Pages stay good for this long: the lists move slowly, and a repeated
/// page (a filter changed, a search retyped) must not touch the network.
const CACHE_TTL: Duration = Duration::from_secs(600);
const CACHE_MAX: usize = 400;
/// After the Store says "too many requests", leave it alone this long.
const PAUSE: Duration = Duration::from_secs(45);

type CacheMap = HashMap<String, (Instant, StorePage)>;
static CACHE: OnceLock<Mutex<CacheMap>> = OnceLock::new();
static PAUSED_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

fn cache() -> std::sync::MutexGuard<'static, CacheMap> {
    CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Forget every cached page (the benchmark measures the network with it).
#[doc(hidden)]
pub fn clear_cache() {
    cache().clear();
}

fn cache_get(key: &str) -> Option<StorePage> {
    let c = cache();
    c.get(key).filter(|(at, _)| at.elapsed() < CACHE_TTL).map(|(_, p)| p.clone())
}

fn cache_put(key: String, page: StorePage) {
    let mut c = cache();
    if c.len() >= CACHE_MAX {
        c.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
        if c.len() >= CACHE_MAX {
            c.clear();
        }
    }
    c.insert(key, (Instant::now(), page));
}

fn paused() -> bool {
    let g = PAUSED_UNTIL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    g.is_some_and(|t| Instant::now() < t)
}

fn pause() {
    *PAUSED_UNTIL.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now() + PAUSE);
}

async fn get_json(url: reqwest::Url) -> Result<Value> {
    let resp = http()?
        .get(url)
        .header("accept", "application/json")
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::other(format!("a loja respondeu HTTP {}", status.as_u16())));
    }
    Ok(resp.json().await?)
}

/// One catalogue page: from the cache when it is fresh, else the Store.
/// An empty or non-JSON body is the end of a list, not an error.
async fn fetch_page(url: reqwest::Url) -> Result<StorePage> {
    let key = url.as_str().to_string();
    if let Some(p) = cache_get(&key) {
        return Ok(p);
    }
    if paused() {
        return Err(Error::other("a loja pediu uma pausa; tente de novo em alguns segundos"));
    }
    let resp = http()?
        .get(url)
        .header("accept", "application/json")
        .send()
        .await?;
    let status = resp.status();
    if matches!(status.as_u16(), 403 | 429) {
        pause();
        return Err(Error::other("a loja limitou as consultas; tente de novo em alguns segundos"));
    }
    if !status.is_success() {
        return Err(Error::other(format!("a loja respondeu HTTP {}", status.as_u16())));
    }
    let body = resp.bytes().await?;
    let page = serde_json::from_slice::<Value>(&body)
        .map(|v| parse_page(&v))
        .unwrap_or_default();
    cache_put(key, page.clone());
    Ok(page)
}

fn search_url(query: &str, cursor: Option<&str>) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse("https://apps.microsoft.com/api/products/search")
        .map_err(|e| Error::other(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("query", query)
        .append_pair("mediaType", "games")
        .append_pair("age", "all")
        .append_pair("price", "all")
        .append_pair("category", "all")
        .append_pair("subscription", "all")
        .append_pair("gl", MARKET)
        .append_pair("hl", LANG);
    if let Some(c) = cursor.filter(|c| !c.is_empty()) {
        url.query_pairs_mut().append_pair("cursor", c);
    }
    Ok(url)
}

/// Games matching `query`, one page at a time.
///
/// # Errors
/// A query shorter than two characters, or a Store failure.
pub async fn search(query: &str, cursor: Option<&str>) -> Result<StorePage> {
    let query = query.trim();
    if query.chars().count() < 2 {
        return Err(Error::other("digite pelo menos 2 letras"));
    }
    fetch_page(search_url(query, cursor)?).await
}

/// Pages of a Store list worth walking: the API answers far past this,
/// but nobody scrolls that deep.
const LIST_PAGES: u32 = 30;

fn list_name(list: &str) -> &'static str {
    match list {
        "TopPaid" => "TopPaid",
        "TopFree" => "TopFree",
        "NewAndRising" => "NewAndRising",
        "TopGrossing" => "TopGrossing",
        _ => "TopTrending",
    }
}

fn list_url(list: &'static str, page_no: u32) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse("https://apps.microsoft.com/api/Reco/GetComputedProductsList")
        .map_err(|e| Error::other(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("listName", list)
        .append_pair("pgNo", &page_no.to_string())
        .append_pair("noItems", "50")
        .append_pair("filteredCategories", "AllProducts")
        .append_pair("mediaType", "Games")
        .append_pair("gl", MARKET)
        .append_pair("hl", LANG)
        .append_pair("exp", "0");
    Ok(url)
}

/// A Store list (`TopTrending`, `TopPaid`, …), one page at a time. The
/// cursor is the next page number.
///
/// # Errors
/// A Store failure.
pub async fn featured(list: &str, cursor: Option<&str>) -> Result<StorePage> {
    let page_no: u32 = cursor.and_then(|c| c.parse().ok()).filter(|n| *n >= 1).unwrap_or(1);
    let mut page = fetch_page(list_url(list_name(list), page_no)?).await?;
    // `hasMorePages` is not reliable on these lists: an empty page is the end.
    page.cursor = (!page.products.is_empty() && page_no < LIST_PAGES).then(|| (page_no + 1).to_string());
    Ok(page)
}

/// Fetch the first screen ahead of time, so opening the store is a cache hit.
pub async fn prewarm() {
    let _ = featured("TopTrending", None).await;
}

/// What the list is narrowed to. Applied here, on the pages as they come,
/// so the phone only ever receives games that pass.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Filter {
    /// Microsoft itself, or a studio it publishes.
    #[serde(default)]
    pub microsoft_only: bool,
    #[serde(default)]
    pub hide_free: bool,
    /// Reais.
    pub min: Option<f64>,
    pub max: Option<f64>,
}

// Microsoft itself and the studios it publishes, as the Store names them.
const MICROSOFT_PUBLISHERS: [&str; 20] = [
    "microsoft", "xbox game studios", "xbox", "bethesda", "activision", "blizzard", "mojang",
    "double fine", "obsidian", "ninja theory", "playground games", "compulsion games",
    "undead labs", "inxile", "halo studios", "343 industries", "the coalition", "turn 10",
    "rare ltd", "world's edge",
];

impl Filter {
    #[must_use]
    pub fn active(&self) -> bool {
        self.microsoft_only || self.hide_free || self.min.is_some() || self.max.is_some()
    }

    #[must_use]
    pub fn accepts(&self, p: &StoreProduct) -> bool {
        if self.microsoft_only && !is_microsoft_published(p.publisher.as_deref()) {
            return false;
        }
        let price = price_of(p);
        if self.hide_free && price == Some(0.0) {
            return false;
        }
        if self.min.is_none() && self.max.is_none() {
            return true;
        }
        let Some(price) = price else { return false };
        self.min.is_none_or(|m| price >= m) && self.max.is_none_or(|m| price <= m)
    }
}

fn is_microsoft_published(publisher: Option<&str>) -> bool {
    let Some(p) = publisher else { return false };
    let p = p.to_lowercase();
    MICROSOFT_PUBLISHERS.iter().any(|m| p.contains(m))
}

/// `249`, `249,90`, `R$ 1.249,00` → reais; blank or junk → `None`.
///
/// Only the number right after `R$`, or a text that is nothing but a
/// number: a price line can carry a date ("25/10/2019"), whose digits
/// read together made a R$ 25 million game.
#[must_use]
pub fn parse_money(text: &str) -> Option<f64> {
    let after = text.find("R$").map(|i| &text[i + 2..]);
    let s = after.unwrap_or(text).trim();
    let num: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.')
        .collect();
    if !num.chars().any(|c| c.is_ascii_digit()) || (after.is_none() && num.len() != s.len()) {
        return None;
    }
    let normal = if num.contains(',') { num.replace('.', "").replace(',', ".") } else { num };
    normal.parse().ok()
}

/// The price in reais; free games are 0, an unpriced one is `None`.
/// `Incluído` (Game Pass) has no price of its own: the list price is the
/// struck-through one.
#[must_use]
pub fn price_of(p: &StoreProduct) -> Option<f64> {
    let text = p.price.as_deref()?;
    let lower = text.to_lowercase();
    if lower.contains("gratuit") || lower.contains("free") {
        return Some(0.0);
    }
    parse_money(text).or_else(|| p.was.as_deref().and_then(parse_money))
}

/// Pages fetched side by side when a filter is on. Few on purpose: the
/// Store answers a burst with 403, and the cache makes repeats free.
const WAVE: u32 = 3;
/// Rounds of fetching one call may make before handing back what it has.
const MAX_ROUNDS: usize = 6;

/// Up to about `want` games that pass `filter`, reading as many pages as
/// that takes. `cursor` continues a previous call (a page number for a
/// list, the Store's own cursor for a search).
///
/// # Errors
/// The Store failed before a single game was found.
pub async fn browse(
    query: &str,
    list: &str,
    cursor: Option<&str>,
    filter: &Filter,
    want: usize,
) -> Result<StorePage> {
    let want = want.clamp(1, 200);
    let query = query.trim();
    let mut out: Vec<StoreProduct> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut take = |batch: Vec<StoreProduct>, out: &mut Vec<StoreProduct>| {
        for p in batch {
            if filter.accepts(&p) && seen.insert(p.product_id.clone()) {
                out.push(p);
            }
        }
    };

    if !query.is_empty() {
        // A search's cursor is opaque and chained: one page after another.
        let mut cur = cursor.map(str::to_string);
        for _ in 0..MAX_ROUNDS {
            let page = match search(query, cur.as_deref()).await {
                Ok(p) => p,
                Err(e) if out.is_empty() => return Err(e),
                Err(_) => break,
            };
            take(page.products, &mut out);
            cur = page.cursor;
            if cur.is_none() || out.len() >= want {
                break;
            }
        }
        return Ok(StorePage { products: out, cursor: cur });
    }

    let list = list_name(list);
    let mut next = Some(cursor.and_then(|c| c.parse().ok()).filter(|n| *n >= 1).unwrap_or(1u32));
    // Without a filter nearly every page fills the screen: one request.
    let wave = if filter.active() { WAVE } else { 1 };
    for _ in 0..MAX_ROUNDS {
        let Some(start) = next else { break };
        let end = (start + wave).min(LIST_PAGES + 1);
        let mut set = tokio::task::JoinSet::new();
        for n in start..end {
            set.spawn(async move {
                let page = match list_url(list, n) {
                    Ok(url) => fetch_page(url).await,
                    Err(e) => Err(e),
                };
                (n, page)
            });
        }
        let mut pages: Vec<(u32, Result<StorePage>)> = Vec::new();
        while let Some(joined) = set.join_next().await {
            if let Ok(r) = joined {
                pages.push(r);
            }
        }
        pages.sort_by_key(|(n, _)| *n);

        next = Some(end).filter(|n| *n <= LIST_PAGES);
        for (n, res) in pages {
            match res {
                Ok(page) if page.products.is_empty() => {
                    next = None;
                    break;
                }
                Ok(page) => take(page.products, &mut out),
                Err(e) if out.is_empty() => return Err(e),
                Err(_) => {
                    next = Some(n);
                    break;
                }
            }
        }
        if out.len() >= want {
            break;
        }
    }
    Ok(StorePage { products: out, cursor: next.map(|n| n.to_string()) })
}

/// The Store's page for a product, where it is bought. The xbox.com
/// store accepts any slug before the id and redirects to the real one.
#[must_use]
pub fn purchase_url(product_id: &str) -> String {
    let id: String = product_id.chars().filter(char::is_ascii_alphanumeric).collect();
    format!("https://www.xbox.com/pt-BR/games/store/p/{}", id.to_uppercase())
}

/// Where the purchase form is posted: the Store's own checkout, the one
/// the Xbox app and xbox.com open in a modal.
pub const BUY_NOW_URL: &str = "https://www.microsoft.com/store/purchase/buynowui/buynow";
/// A page on the same origin to post from: the checkout answers a form
/// with `Origin: null` with an empty 403.
pub const BUY_NOW_HOST_URL: &str = "https://www.microsoft.com/store/purchase/buynowui/prefetch/buynow";

/// The checkout's form for one product, signed in by `xToken` alone.
#[derive(Debug, Clone, Serialize)]
pub struct PurchaseForm {
    pub host: String,
    pub action: String,
    pub fields: Vec<(String, String)>,
}

/// The SKU and offer the checkout needs for a product: its first SKU
/// with a `Purchase` availability.
async fn purchase_offer(product_id: &str) -> Result<(String, String)> {
    let url = reqwest::Url::parse_with_params(
        "https://displaycatalog.mp.microsoft.com/v7.0/products",
        &[("bigIds", product_id), ("market", MARKET), ("languages", LANG)],
    )
    .map_err(|e| Error::other(e.to_string()))?;
    let v = get_json(url).await?;
    v["Products"][0]["DisplaySkuAvailabilities"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|dsa| {
            let sku = dsa["Sku"]["SkuId"].as_str()?;
            let avail = dsa["Availabilities"].as_array()?.iter().find(|a| {
                a["Actions"]
                    .as_array()
                    .is_some_and(|acts| acts.iter().any(|x| x == "Purchase"))
            })?;
            Some((sku.to_string(), avail["AvailabilityId"].as_str()?.to_string()))
        })
        .ok_or_else(|| Error::other("este produto não está à venda"))
}

/// The checkout form for `product_id`, as the Xbox app posts it.
///
/// # Errors
/// The product has no purchasable offer, or the catalogue failed.
pub async fn purchase_form(product_id: &str, x_token: &str) -> Result<PurchaseForm> {
    let id: String = product_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_uppercase();
    let (sku, availability) = purchase_offer(&id).await?;
    let products = serde_json::json!([{ "productId": id, "skuId": sku, "availabilityId": availability }]);
    let flights = [
        "sc_xboxspinner",
        "sc_xboxclosebutton",
        "sc_xboxgamepad",
        "sc_xboxuiexp",
        "sc_disabledefaultstyles",
        "sc_gamertaggifting",
    ];
    let mut fields = vec![
        ("data".into(), r#"{"usePurchaseSdk":true}"#.into()),
        ("market".into(), MARKET.into()),
        ("cV".into(), String::new()),
        ("locale".into(), LANG.into()),
        ("xToken".into(), x_token.into()),
        ("pageFormat".into(), "full".into()),
        ("products".into(), products.to_string()),
        ("campaignId".into(), "xboxcomct".into()),
        ("callerApplicationId".into(), "XboxCom".into()),
        (
            "expId".into(),
            flights.iter().map(|f| format!("EX:{f}")).collect::<Vec<_>>().join(","),
        ),
    ];
    fields.extend(flights.iter().enumerate().map(|(i, f)| (format!("flights[{i}]"), (*f).to_string())));
    fields.extend([
        ("clientType".into(), "XboxCom".into()),
        ("layout".into(), "Modal".into()),
        ("cssOverride".into(), "XboxCom2NewUI".into()),
        ("theme".into(), "dark".into()),
        ("scenario".into(), String::new()),
        ("suppressGiftThankYouPage".into(), "false".into()),
    ]);
    Ok(PurchaseForm {
        host: BUY_NOW_HOST_URL.into(),
        action: format!("{BUY_NOW_URL}?noCanonical=true&market={MARKET}&locale={LANG}"),
        fields,
    })
}

#[doc(hidden)]
pub fn parse_page(v: &Value) -> StorePage {
    let products = v["productsList"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(parse_product)
        .collect();
    let cursor = (v["hasMorePages"].as_bool() == Some(true))
        .then(|| v["cursor"].as_str().map(str::to_string))
        .flatten()
        .filter(|c| !c.is_empty());
    StorePage { products, cursor }
}

fn parse_product(p: &Value) -> Option<StoreProduct> {
    let product_id = p["productId"].as_str()?.trim().to_uppercase();
    let title = p["title"].as_str()?.trim().to_string();
    if product_id.is_empty() || title.is_empty() {
        return None;
    }
    let price_info = &p["priceInfo"];
    let price = price_info["displayPrice"]
        .as_str()
        .or_else(|| p["displayPrice"].as_str())
        .or_else(|| p["price"]["displayPrice"].as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty());
    let was = p["strikethroughPrice"]
        .as_str()
        .or_else(|| price_info["strikethroughPrice"].as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty() && Some(s) != price.as_ref());
    Some(StoreProduct {
        product_id,
        title,
        publisher: p["publisherName"].as_str().map(str::to_string),
        image: https(p["boxArtUrl"].as_str()).or_else(|| https(p["posterArtUrl"].as_str())),
        price,
        was,
        pass: p["plaintextPassName"].as_str().map(str::to_string).filter(|s| !s.is_empty()),
        rating: p["averageRating"].as_f64().filter(|r| *r > 0.0),
    })
}

/// Store art, https only (Android refuses cleartext) and sized for a card.
fn https(raw: Option<&str>) -> Option<String> {
    let rest = raw?
        .strip_prefix("https://")
        .or_else(|| raw?.strip_prefix("http://"))?;
    if rest.is_empty() {
        return None;
    }
    let sep = if rest.contains('?') { '&' } else { '?' };
    Some(format!("https://{rest}{sep}w=360"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn game(publisher: &str, price: &str, was: Option<&str>) -> StoreProduct {
        StoreProduct {
            product_id: "X".into(),
            title: "T".into(),
            publisher: Some(publisher.into()),
            price: Some(price.into()),
            was: was.map(str::to_string),
            ..StoreProduct::default()
        }
    }

    #[test]
    fn money_is_read_the_brazilian_way() {
        assert_eq!(parse_money("R$ 1.249,00"), Some(1249.0));
        assert_eq!(parse_money("249,90"), Some(249.9));
        assert_eq!(parse_money("500"), Some(500.0));
        assert_eq!(parse_money("Incluído"), None);
        assert_eq!(parse_money(""), None);
        assert_eq!(parse_money("25/10/2019"), None, "a date is not a price");
        assert_eq!(parse_money("Disponível em 25/10/2019"), None);
        assert_eq!(parse_money("A partir de R$ 59,90"), Some(59.9));
    }

    #[test]
    fn game_pass_games_are_priced_by_their_list_price() {
        assert_eq!(price_of(&game("x", "Incluído", Some("R$ 149,00"))), Some(149.0));
        assert_eq!(price_of(&game("x", "Gratuito", None)), Some(0.0));
        assert_eq!(price_of(&game("x", "Incluído", None)), None);
    }

    #[test]
    fn filters_narrow_by_publisher_and_price() {
        let ms = game("Xbox Game Studios", "R$ 249,00", None);
        let other = game("Capcom", "R$ 99,00", None);
        let free = game("Microsoft Corporation", "Gratuito", None);
        let f = Filter { microsoft_only: true, ..Filter::default() };
        assert!(f.accepts(&ms) && f.accepts(&free) && !f.accepts(&other));
        let f = Filter { hide_free: true, ..Filter::default() };
        assert!(f.accepts(&ms) && f.accepts(&other) && !f.accepts(&free));
        let f = Filter { min: Some(100.0), max: Some(300.0), ..Filter::default() };
        assert!(f.accepts(&ms) && !f.accepts(&other) && !f.accepts(&free));
        assert!(!f.accepts(&game("x", "Incluído", None)), "unpriced never passes a price range");
        assert!(Filter::default().accepts(&other) && !Filter::default().active());
    }

    #[test]
    fn a_search_result_becomes_a_card() {
        let v = json!({
            "productsList": [{
                "productId": "9nkx70bbcdrn",
                "title": "Forza Horizon 5 Edição Padrão",
                "publisherName": "Xbox Game Studios",
                "boxArtUrl": "https://store-images.s-microsoft.com/image/apps.1",
                "priceInfo": {"displayPrice": "R$ 249,00"},
                "plaintextPassName": "Xbox Game Pass",
                "averageRating": 4.6
            }, {"title": "no id"}],
            "hasMorePages": true,
            "cursor": "abc"
        });
        let page = parse_page(&v);
        assert_eq!(page.products.len(), 1);
        let p = &page.products[0];
        assert_eq!(p.product_id, "9NKX70BBCDRN");
        assert_eq!(p.price.as_deref(), Some("R$ 249,00"));
        assert_eq!(p.pass.as_deref(), Some("Xbox Game Pass"));
        assert_eq!(p.image.as_deref(), Some("https://store-images.s-microsoft.com/image/apps.1?w=360"));
        assert_eq!(page.cursor.as_deref(), Some("abc"));
    }

    #[test]
    fn the_last_page_has_no_cursor() {
        let page = parse_page(&json!({"productsList": [], "hasMorePages": false, "cursor": "x"}));
        assert!(page.cursor.is_none());
    }

    #[test]
    fn a_purchase_goes_to_the_products_store_page() {
        assert_eq!(
            purchase_url("9nkx70bbcdrn"),
            "https://www.xbox.com/pt-BR/games/store/p/9NKX70BBCDRN"
        );
        assert_eq!(purchase_url("9N/../x"), "https://www.xbox.com/pt-BR/games/store/p/9NX");
    }

    /// Live: the Store answers and parses. `cargo test -p xau-mobile store_live -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn store_live() {
        let page = search("forza", None).await.expect("search");
        assert!(!page.products.is_empty());
        let top = featured("TopTrending", None).await.expect("featured");
        assert!(!top.products.is_empty());
    }
}
