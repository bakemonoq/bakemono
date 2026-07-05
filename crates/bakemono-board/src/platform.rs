use anyhow::{bail, Context, Result};
use serde_json::Value;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0 Safari/537.36";

// the platforms the contribute form offers; the tuple is (id, label, session cookie name, cookie domain)
pub const PLATFORMS: &[(&str, &str, &str, &str)] = &[
    ("patreon", "Patreon", "session_id", "patreon.com"),
    ("fanbox", "Fanbox", "FANBOXSESSID", "fanbox.cc"),
    ("boosty", "Boosty", "auth", "boosty.to"),
    ("gumroad", "Gumroad", "_gumroad_app_session", "gumroad.com"),
    ("subscribestar", "SubscribeStar", "_personalization_id", "subscribestar.adult"),
    ("fantia", "Fantia", "_session_id", "fantia.jp"),
    ("afdian", "Afdian", "auth_token", "afdian.com"),
];

pub fn is_known(platform: &str) -> bool {
    PLATFORMS.iter().any(|p| p.0 == platform)
}

pub fn label(platform: &str) -> &str {
    PLATFORMS.iter().find(|p| p.0 == platform).map(|p| p.1).unwrap_or(platform)
}

fn spec(platform: &str) -> Option<(&'static str, &'static str)> {
    PLATFORMS.iter().find(|p| p.0 == platform).map(|p| (p.2, p.3))
}

// a Netscape cookies.txt gallery-dl can read, holding just the session cookie
pub fn netscape_cookie(platform: &str, token: &str) -> Option<String> {
    let (name, domain) = spec(platform)?;
    Some(format!(
        "# Netscape HTTP Cookie File\n.{domain}\tTRUE\t/\tTRUE\t9999999999\t{name}\t{token}\n"
    ))
}

pub struct Discovered {
    pub id: String,
    pub name: String,
    pub url: String,
}

// Ok(Some(list)) = cookie is live (list may be empty); Ok(None) = cookie rejected (dead);
// Err = transport/unsupported, status unchanged
pub async fn discover(platform: &str, token: &str) -> Result<Option<Vec<Discovered>>> {
    match platform {
        "patreon" => discover_patreon(token).await,
        "fanbox" => discover_fanbox(token).await,
        other => bail!("auto-discovery for {} is not available yet", label(other)),
    }
}

async fn discover_patreon(token: &str) -> Result<Option<Vec<Discovered>>> {
    let client = client()?;
    let cookie = format!("session_id={token}");

    // authenticate first: no data.id means the cookie is dead, distinct from a live cookie with no pledges
    let me = client
        .get("https://www.patreon.com/api/current_user?json-api-version=1.0")
        .header("Cookie", &cookie)
        .send()
        .await
        .context("reaching patreon")?;
    if me.status() == 401 || me.status() == 403 {
        return Ok(None);
    }
    if !me.status().is_success() {
        bail!("patreon returned {}", me.status());
    }
    let me: Value = me.json().await.context("patreon response not JSON")?;
    if me["data"]["id"].as_str().is_none() {
        return Ok(None);
    }

    // pledges is the authoritative subscription list; memberships.campaign catches the newer shape.
    // merge and dedup, since which one is populated varies by account and api version
    let mut out: Vec<Discovered> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for url in [
        "https://www.patreon.com/api/pledges?include=campaign&fields[campaign]=name,url,vanity&json-api-version=1.0",
        "https://www.patreon.com/api/current_user?include=memberships.campaign&fields[campaign]=name,url,vanity&json-api-version=1.0",
    ] {
        let Ok(resp) = client.get(url).header("Cookie", &cookie).send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(body) = resp.json::<Value>().await else { continue };
        for item in body["included"].as_array().into_iter().flatten() {
            if item["type"] != "campaign" {
                continue;
            }
            let id = item["id"].as_str().unwrap_or_default().to_string();
            if id.is_empty() || !seen.insert(id.clone()) {
                continue;
            }
            let attrs = &item["attributes"];
            let name = attrs["name"].as_str().unwrap_or_default().to_string();
            let url = attrs["url"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| attrs["vanity"].as_str().map(|v| format!("https://www.patreon.com/{v}")))
                .unwrap_or_else(|| format!("https://www.patreon.com/user?u={id}"));
            out.push(Discovered { id, name, url });
        }
    }
    Ok(Some(out))
}

async fn discover_fanbox(token: &str) -> Result<Option<Vec<Discovered>>> {
    let resp = client()?
        .get("https://api.fanbox.cc/plan.listSupporting")
        .header("Cookie", format!("FANBOXSESSID={token}"))
        .header("Origin", "https://www.fanbox.cc")
        .send()
        .await
        .context("reaching fanbox")?;
    if resp.status() == 401 {
        return Ok(None);
    }
    if !resp.status().is_success() {
        bail!("fanbox returned {}", resp.status());
    }
    let body: Value = resp.json().await.context("fanbox response not JSON")?;
    let Some(plans) = body["body"].as_array() else {
        return Ok(None);
    };
    let mut out = Vec::new();
    for plan in plans {
        let id = plan["creatorId"].as_str().unwrap_or_default().to_string();
        let name = plan["user"]["name"].as_str().unwrap_or_default().to_string();
        if !id.is_empty() {
            out.push(Discovered { url: format!("https://{id}.fanbox.cc"), id, name });
        }
    }
    Ok(Some(out))
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(UA)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building http client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netscape_has_the_right_cookie_name() {
        let c = netscape_cookie("patreon", "TOK").unwrap();
        assert!(c.contains("session_id\tTOK"));
        assert!(c.contains(".patreon.com"));
        assert!(netscape_cookie("nope", "x").is_none());
    }

    #[tokio::test]
    async fn unsupported_platform_errors() {
        assert!(discover("gumroad", "x").await.is_err());
    }
}
