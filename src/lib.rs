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
struct CachedData {
    timestamp: u64,
    forecast: NwsForecast,
}

// New struct to track the paywall TTL
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

    // ==========================================
    // 1. THE 5-MINUTE TTL EDGE PAYWALL
    // ==========================================
    let counter_store = Store::open("counters")?;
    
    // Retrieve or initialize the user's paywall state
    let mut user_data = match counter_store.get(client_ip)? {
        Some(bytes) => serde_json::from_slice::<PaywallData>(&bytes).unwrap_or(PaywallData { count: 0, timestamp: now }),
        None => PaywallData { count: 0, timestamp: now },
    };

    // If more than 5 minutes (300 seconds) have passed, reset the counter
    if now - user_data.timestamp > 300 {
        user_data.count = 0;
        user_data.timestamp = now; // Start the clock over on their next free batch
    }

    // Block if they have hit the route 3 times within the 5-minute window
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

    // Increment, calculate views left, and save back to KV
    user_data.count += 1;
    let views_left = 3 - user_data.count;
    counter_store.set(client_ip, &serde_json::to_vec(&user_data)?)?;

    // ==========================================
    // 2. GEO-IP & WEATHER LOGIC (Unchanged)
    // ==========================================
    let geo_url = if raw_ip.is_empty() {
        "http://ip-api.com/json/".to_string()
    } else {
        format!("http://ip-api.com/json/{}", raw_ip)
    };
    let geo_res: Response = send(Request::get(geo_url)).await?;
    let geo: Geo = serde_json::from_slice(geo_res.body())?;

    let cache_store = Store::open_default()?;
    let cache_key = format!("forecast_{}", geo.zip);
    
    let mut final_forecast: Option<NwsForecast> = None;

    if let Ok(Some(cached_bytes)) = cache_store.get(&cache_key) {
        if let Ok(cached_wrapper) = serde_json::from_slice::<CachedData>(&cached_bytes) {
            if now - cached_wrapper.timestamp < 300 {
                final_forecast = Some(cached_wrapper.forecast);
            }
        }
    }

    if final_forecast.is_none() {
        let points_url = format!("https://api.weather.gov/points/{},{}", geo.lat, geo.lon);
        let points_req = Request::builder().method(Method::Get).uri(points_url).header("User-Agent", "Akamai-Functions-Demo/1.0").build();
        let points_res: Response = send(points_req).await?;
        let points: NwsPoints = serde_json::from_slice(points_res.body())?;

        let forecast_req = Request::builder().method(Method::Get).uri(points.properties.forecast).header("User-Agent", "Akamai-Functions-Demo/1.0").build();
        let forecast_res: Response = send(forecast_req).await?;
        let fresh_forecast: NwsForecast = serde_json::from_slice(forecast_res.body())?;

        let cache_wrapper = CachedData { timestamp: now, forecast: fresh_forecast.clone() };
        cache_store.set(&cache_key, &serde_json::to_vec(&cache_wrapper)?)?;
        final_forecast = Some(fresh_forecast);
    }

    let forecast = final_forecast.unwrap();

    // ==========================================
    // 3. PRIVATE NESTED FUNCTION CALL 
    // ==========================================
    // This utilizes Spin's internal service chaining, bypassing public networks
    let trending_req = Request::get("http://trending.spin.internal");
    let mut trending_html = String::new();
    if let Ok(mut resp) = send(trending_req).await {
        trending_html = String::from_utf8_lossy(&resp.body_mut().take().unwrap_or_default()).to_string();
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
