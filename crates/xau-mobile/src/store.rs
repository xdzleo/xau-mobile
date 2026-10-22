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

use std::time::Duration;

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

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?)
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

/// Games matching `query`, one page at a time.
///
/// # Errors
/// A query shorter than two characters, or a Store failure.
pub async fn search(query: &str, cursor: Option<&str>) -> Result<StorePage> {
    let query = query.trim();
    if query.chars().count() < 2 {
        return Err(Error::other("digite pelo menos 2 letras"));
    }
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
    Ok(parse_page(&get_json(url).await?))
}

/// What is trending in the Store right now.
///
/// # Errors
/// A Store failure.
pub async fn featured() -> Result<StorePage> {
    let mut url = reqwest::Url::parse("https://apps.microsoft.com/api/Reco/GetComputedProductsList")
        .map_err(|e| Error::other(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("listName", "TopTrending")
        .append_pair("pgNo", "1")
        .append_pair("noItems", "24")
        .append_pair("filteredCategories", "AllProducts")
        .append_pair("mediaType", "Games")
        .append_pair("gl", MARKET)
        .append_pair("hl", LANG)
        .append_pair("exp", "0");
    let mut page = parse_page(&get_json(url).await?);
    page.cursor = None;
    Ok(page)
}

/// The Store's page for a product, where it is bought. The xbox.com
/// store accepts any slug before the id and redirects to the real one.
#[must_use]
pub fn purchase_url(product_id: &str) -> String {
    let id: String = product_id.chars().filter(char::is_ascii_alphanumeric).collect();
    format!("https://www.xbox.com/pt-BR/games/store/p/{}", id.to_uppercase())
}

fn parse_page(v: &Value) -> StorePage {
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
        let top = featured().await.expect("featured");
        assert!(!top.products.is_empty());
    }
}
