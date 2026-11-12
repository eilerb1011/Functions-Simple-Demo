use anyhow::Result;
use spin_sdk::http::{IntoResponse, Request, Response, Method, send};
use spin_sdk::http_component;
use spin_sdk::key_value::Store; 
use serde::{Deserialize, Serialize}; 
use std::time::{SystemTime, UNIX_EPOCH};

#[allow(non_snake_case)]
#[derive(Deserialize)]
struct Geo { lat: f64, lon: f64, city: String, region: String, zip: String }

#[allow(non_snake_case)]
#[derive(Deserialize)]
struct NwsPoints { properties: NwsPointsProps }

#[allow(non_snake_case)]
#[derive(Deserialize)]
struct NwsPointsProps { forecast: String }

#[allow(non_snake_case)]
#[derive(Serialize, Deserialize, Clone)]
struct NwsForecast { properties: NwsForecastProps }

#[allow(non_snake_case)]
#[derive(Serialize, Deserialize, Clone)]
struct NwsForecastProps { periods: Vec<Period> }

#[allow(non_snake_case)]
#[derive(Serialize, Deserialize, Clone)]
struct Period { 
    name: String, 
    temperature: i32, 
    temperatureUnit: String, 
    shortForecast: String, 
    detailedForecast: String,
    icon: String
}

#[derive(Serialize, Deserialize)]
struct PaywallData {
    count: i32,
    timestamp: u64,
}

#[http_component]
async fn handle_weather_request(req: Request) -> Result<impl IntoResponse> {
    let raw_ip = req.header("x-forwarded-for")
        .or_else(|| req.header("true-client-ip"))
        .and_then(|h| h.as_str())
        .map(|s| s.split(',').next().unwrap_or(s).trim())
        .unwrap_or(""); 
        
    let client_ip = if raw_ip.is_empty() { "local_dev" } else { raw_ip };
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    let store = Store::open_default()?;

    // ==========================================
    // 1. THE 5-MINUTE TTL EDGE PAYWALL
    // ==========================================
    let paywall_key = format!("paywall_{}", client_ip);
    
    let mut user_data = match store.get(&paywall_key)? {
        Some(bytes) => serde_json::from_slice::<PaywallData>(&bytes).unwrap_or(PaywallData { count: 0, timestamp: now }),
        None => PaywallData { count: 0, timestamp: now },
    };

    if now - user_data.timestamp > 300 {
        user_data.count = 0;
        user_data.timestamp = now; 
    }

    if user_data.count >= 3 {
        let seconds_left = 300 - (now - user_data.timestamp);
        let paywall_html = format!(
            "<html><body style='background:#111; color:white; font-family:sans-serif; text-align:center; padding-top:20vh;'>
            <h1>Limit Reached</h1>
            <p>You have used your 3 free edge requests.</p>
            <p>Access resets in <b>{} seconds</b>.</p>
            </body></html>", seconds_left
        );
        return Ok(Response::builder().status(403).header("content-type", "text/html").body(paywall_html).build());
    }

    user_data.count += 1;
    let views_left = 3 - user_data.count;
    store.set(&paywall_key, &serde_json::to_vec(&user_data)?)?;

    // ==========================================
    // 2. GEO-IP & SHARDED WEATHER CACHE
    // ==========================================
    let geo_url = if raw_ip.is_empty() {
        "http://ip-api.com/json/".to_string()
    } else {
        format!("http://ip-api.com/json/{}", raw_ip)
    };
    let geo_res: Response = send(Request::get(geo_url)).await?;
    let geo: Geo = serde_json::from_slice(geo_res.body())?;

    let meta_key = format!("forecast_{}_meta", geo.zip);
    let mut cached_periods: Vec<Period> = Vec::new();
    let mut is_cache_valid = false;

    // A. Check the Meta Key for freshness
    if let Ok(Some(meta_bytes)) = store.get(&meta_key) {
        if let Ok(timestamp_str) = std::str::from_utf8(&meta_bytes) {
            if let Ok(timestamp) = timestamp_str.parse::<u64>() {
                if now - timestamp < 300 {
                    is_cache_valid = true;
                }
            }
        }
    }

    // B. Reassemble from multiple keys if valid
    if is_cache_valid {
        for i in 0..6 {
            let period_key = format!("forecast_{}_p{}", geo.zip, i);
            if let Ok(Some(p_bytes)) = store.get(&period_key) {
                if let Ok(period) = serde_json::from_slice::<Period>(&p_bytes) {
                    cached_periods.push(period);
                }
            }
        }
        if cached_periods.len() != 6 {
            is_cache_valid = false; // Fallback if a key is missing
        }
    }

    // C. Cache Miss vs Cache Hit determination
    let forecast = if is_cache_valid {
        println!("CACHE HIT for Zip: {}", geo.zip);
        NwsForecast { properties: NwsForecastProps { periods: cached_periods } }
    } else {
        println!("CACHE MISS. Fetching fresh data for Zip: {}", geo.zip);
        let points_url = format!("https://api.weather.gov/points/{},{}", geo.lat, geo.lon);
        let points_req = Request::builder().method(Method::Get).uri(points_url).header("User-Agent", "Akamai-Functions-Demo/1.0").build();
        let points_res: Response = send(points_req).await?;
        let points: NwsPoints = serde_json::from_slice(points_res.body())?;

        let forecast_req = Request::builder().method(Method::Get).uri(points.properties.forecast).header("User-Agent", "Akamai-Functions-Demo/1.0").build();
        let forecast_res: Response = send(forecast_req).await?;
        let mut fresh_forecast: NwsForecast = serde_json::from_slice(forecast_res.body())?;

        // Write Meta Timestamp
        store.set(&meta_key, now.to_string().as_bytes())?;

        // Write each period to its own isolated 300-byte key
        fresh_forecast.properties.periods.truncate(6);
        for (i, period) in fresh_forecast.properties.periods.iter().enumerate() {
            let period_key = format!("forecast_{}_p{}", geo.zip, i);
            let mut p_clone = period.clone();
            if i > 0 { p_clone.detailedForecast.clear(); } // Clean up unused strings
            store.set(&period_key, &serde_json::to_vec(&p_clone)?)?;
        }
        fresh_forecast
    };

    // ==========================================
    // 3. PRIVATE NESTED FUNCTION CALL 
    // ==========================================
    let trending_req = Request::get("http://trending.spin.internal");
    let mut trending_html = String::new();
    
    // Explicitly annotate the type so the compiler knows this returns a Response
    let trending_res: Result<Response, _> = send(trending_req).await;
    
    if let Ok(resp) = trending_res {
        trending_html = String::from_utf8_lossy(resp.body()).to_string();
    }
    // ==========================================
    // 4. HTML RENDERING
    // ==========================================
    let current = &forecast.properties.periods[0];
    let condition = current.shortForecast.to_lowercase();
    
    let bg_image = if condition.contains("rain") || condition.contains("shower") || condition.contains("drizzle") {
        "https://images.unsplash.com/photo-1515694346937-94d85e41e6f0?auto=format&fit=crop&w=1920&q=80"
    } else if condition.contains("snow") || condition.contains("ice") || condition.contains("flurries") {
        "https://images.unsplash.com/photo-1478265409131-1f65c88f965c?auto=format&fit=crop&w=1920&q=80"
    } else if condition.contains("thunder") || condition.contains("storm") {
        "https://images.unsplash.com/photo-1605727216801-e27ce1d0ce49?auto=format&fit=crop&w=1920&q=80"
    } else if condition.contains("cloud") || condition.contains("overcast") {
        "https://images.unsplash.com/photo-1534088568595-a066f410cbda?auto=format&fit=crop&w=1920&q=80"
    } else {
        "https://images.unsplash.com/photo-1604228741406-3faa38f4907a?auto=format&fit=crop&w=1920&q=80"
    };

    let mut html = format!(
        "<html>
        <head>
            <title>Weather for {}</title>
            <style>
                body {{ margin: 0; font-family: 'Segoe UI', sans-serif; background-image: url('{}'); background-size: cover; background-position: center; background-attachment: fixed; color: #ffffff; display: flex; justify-content: center; align-items: center; min-height: 100vh; }}
                .card {{ background: rgba(0, 0, 0, 0.7); backdrop-filter: blur(8px); padding: 2.5rem; border-radius: 16px; max-width: 600px; width: 90%; box-shadow: 0 10px 30px rgba(0,0,0,0.5); position: relative; }}
                .badge {{ position: absolute; top: 20px; right: 20px; background: #b30000; padding: 5px 10px; border-radius: 5px; font-weight: bold; font-size: 0.8rem; }}
                h1 {{ margin-top: 0; font-size: 2.2rem; }} 
                h2 {{ color: #4dabf7; margin: 0; }} 
                .current-weather {{ display: flex; align-items: center; gap: 20px; margin-bottom: 1rem; }}
                .current-weather img {{ border-radius: 10px; box-shadow: 0 4px 10px rgba(0,0,0,0.3); width: 86px; height: 86px; }}
                ul {{ list-style-type: none; padding: 0; }}
                li {{ background: rgba(255, 255, 255, 0.1); margin: 12px 0; padding: 15px; border-radius: 8px; border-left: 4px solid #4dabf7; display: flex; align-items: center; gap: 15px; }}
                li img {{ border-radius: 6px; width: 50px; height: 50px; }}
                .trending {{ margin-top: 30px; padding-top: 20px; border-top: 1px solid rgba(255,255,255,0.2); }}
                .trending h3 {{ color: #ffd700; margin-top: 0; }}
            </style>
        </head>
        <body>
            <div class='card'>
                <div class='badge'>{} Free Views Left</div>
                <h1>{}, {}</h1>
                <div class='current-weather'>
                    <img src='{}' alt='{}'>
                    <div><h2>{}°{} - {}</h2></div>
                </div>
                <p>{}</p>
                <hr style='border: 1px solid rgba(255,255,255,0.2); margin: 20px 0;'>
                <h3>5-Day Outlook</h3><ul>",
        geo.city, bg_image, views_left, geo.city, geo.region, 
        current.icon, current.shortForecast,
        current.temperature, current.temperatureUnit, current.shortForecast, current.detailedForecast
    );

    for period in forecast.properties.periods.iter().take(6).skip(1) {
        html.push_str(&format!(
            "<li><img src='{}' alt='{}'><div><strong>{}:</strong> {}°{} - {}</div></li>", 
            period.icon, period.shortForecast, period.name, period.temperature, period.temperatureUnit, period.shortForecast
        ));
    }
    
    html.push_str(&format!("</ul><div class='trending'><h3>Sponsored Links</h3>{}</div></div></body></html>", trending_html));

    Ok(Response::builder().status(200).header("content-type", "text/html; charset=utf-8").body(html).build())
}

fn main() {}
