//! Bounded metrics and spend reads (war-room slice 8).

use aether_bloomery::{
    BloomId, DAYS_CAP, METRICS_DEFAULT_LIMIT, METRICS_MAX_LIMIT, MetricBloom, MetricDay, MetricDispatch, MetricsQuery,
    MetricsQueryResult, MetricsSeat, MetricsSummary, MetricsTimeline, MetricsView, SpendQuery, SpendQueryResult,
    SpendWindow,
};
use aether_data::wire::from_bytes;
use aether_http::HttpServerResponse;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::hex::digest_from_hex;
use super::response::{error_response, json};
use super::state::Routed;

/// `GET /metrics/summary`
pub(super) fn summary() -> Routed {
    Routed::Metrics(MetricsQuery { view: MetricsView::Summary, bloom: None, from_sequence: None, limit: None })
}

/// `GET /metrics/days`
pub(super) fn days() -> Routed {
    Routed::Metrics(MetricsQuery { view: MetricsView::Days, bloom: None, from_sequence: None, limit: Some(DAYS_CAP) })
}

/// `GET /metrics/blooms`
pub(super) fn blooms(query: &str) -> Result<Routed, String> {
    let (from_sequence, limit) = parse_page(query)?;
    Ok(Routed::Metrics(MetricsQuery { view: MetricsView::Blooms, bloom: None, from_sequence, limit }))
}

/// `GET /metrics/blooms/{id}/timeline`
pub(super) fn timeline(id: &str) -> Result<Routed, String> {
    let bloom = digest_from_hex(id).ok_or_else(|| format!("bloom is not a 64-character hex digest: {id}"))?;
    Ok(Routed::Metrics(MetricsQuery {
        view: MetricsView::Timeline,
        bloom: Some(BloomId(bloom).0.as_bytes().to_vec()),
        from_sequence: None,
        limit: None,
    }))
}

/// `GET /metrics/seats`
pub(super) fn seats() -> Routed {
    Routed::Metrics(MetricsQuery { view: MetricsView::Seats, bloom: None, from_sequence: None, limit: None })
}

/// `GET /metrics/dispatches`
pub(super) fn dispatches(query: &str) -> Result<Routed, String> {
    let (from_sequence, limit) = parse_page(query)?;
    Ok(Routed::Metrics(MetricsQuery { view: MetricsView::Dispatches, bloom: None, from_sequence, limit }))
}

/// `GET /spend`
pub(super) fn spend() -> Routed {
    Routed::Spend(SpendQuery)
}

pub(super) fn metrics_response(result: MetricsQueryResult) -> HttpServerResponse {
    match result {
        MetricsQueryResult::Ok { document } => render(&document),
        MetricsQueryResult::NotFound => error_response(404, "no such bloom"),
        MetricsQueryResult::Err { error } => error_response(400, &error),
    }
}

pub(super) fn spend_response(result: SpendQueryResult) -> HttpServerResponse {
    match result {
        SpendQueryResult::Ok { window } => match decode::<SpendWindow>(&window) {
            Ok(value) => json(200, &value),
            Err(error) => error_response(500, &error),
        },
        SpendQueryResult::Err { error } => error_response(500, &error),
    }
}

fn render(bytes: &[u8]) -> HttpServerResponse {
    if let Ok(value) = decode::<MetricsSummary>(bytes) {
        return json(200, &value);
    }
    if let Ok(value) = decode::<Vec<MetricDay>>(bytes) {
        return json(200, &value);
    }
    if let Ok(value) = decode::<Vec<MetricBloom>>(bytes) {
        return json(200, &value);
    }
    if let Ok(value) = decode::<MetricsTimeline>(bytes) {
        return json(200, &value);
    }
    if let Ok(value) = decode::<Vec<MetricsSeat>>(bytes) {
        return json(200, &value);
    }
    if let Ok(value) = decode::<Vec<MetricDispatch>>(bytes) {
        return json(200, &value);
    }
    error_response(500, "metrics document decode failed")
}

fn decode<T: DeserializeOwned + Serialize>(bytes: &[u8]) -> Result<T, String> {
    from_bytes::<T>(bytes).map_err(|error| format!("metrics document decode failed: {error}"))
}

fn parse_page(query: &str) -> Result<(Option<u64>, Option<u64>), String> {
    let mut from_sequence = None;
    let mut requested = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "from_sequence" => {
                from_sequence =
                    Some(value.parse::<u64>().map_err(|_| format!("from_sequence is not an integer: {value}"))?);
            }
            "limit" => {
                requested = Some(value.parse::<u64>().map_err(|_| format!("limit is not an integer: {value}"))?);
            }
            _ => {}
        }
    }
    let limit = match requested {
        None => Some(METRICS_DEFAULT_LIMIT),
        Some(limit) if limit > METRICS_MAX_LIMIT => Some(METRICS_MAX_LIMIT),
        Some(limit) => Some(limit),
    };
    Ok((from_sequence, limit))
}
