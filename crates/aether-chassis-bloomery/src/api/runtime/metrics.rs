//! Bounded metrics and spend reads (war-room slice 8).

use aether_bloomery::{
    BloomId, DAYS_CAP, METRICS_DEFAULT_LIMIT, METRICS_MAX_LIMIT, MetricBloom, MetricDay, MetricDispatch, MetricsQuery,
    MetricsQueryResult, MetricsSeat, MetricsSummary, MetricsTimeline, MetricsView, SpendQuery, SpendQueryResult,
    SpendWindow,
};
use aether_data::wire::from_bytes;
use aether_http::{HttpHeader, HttpServerResponse};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::hex::digest_from_hex;
use super::reads::{clamp_limit, pairs, parse_u64};
use super::response::{error_response, json};
use super::state::Routed;

/// `GET /metrics/summary`
pub(super) fn summary() -> Routed {
    Routed::Metrics(metrics_query(MetricsView::Summary, None, None, None, None))
}

/// `GET /metrics/days`
pub(super) fn days() -> Routed {
    Routed::Metrics(metrics_query(MetricsView::Days, None, None, Some(DAYS_CAP), None))
}

/// `GET /metrics/blooms`
pub(super) fn blooms(query: &str) -> Result<Routed, String> {
    let (from_sequence, limit, notice) = parse_page(query)?;
    Ok(Routed::Metrics(metrics_query(MetricsView::Blooms, None, from_sequence, limit, notice)))
}

/// `GET /metrics/blooms/{id}/timeline`
pub(super) fn timeline(id: &str) -> Result<Routed, String> {
    let bloom = digest_from_hex(id).ok_or_else(|| format!("bloom is not a 64-character hex digest: {id}"))?;
    Ok(Routed::Metrics(metrics_query(
        MetricsView::Timeline,
        Some(BloomId(bloom).0.as_bytes().to_vec()),
        None,
        None,
        None,
    )))
}

/// `GET /metrics/seats`
pub(super) fn seats() -> Routed {
    Routed::Metrics(metrics_query(MetricsView::Seats, None, None, None, None))
}

/// `GET /metrics/dispatches`
pub(super) fn dispatches(query: &str) -> Result<Routed, String> {
    let (from_sequence, limit, notice) = parse_page(query)?;
    Ok(Routed::Metrics(metrics_query(MetricsView::Dispatches, None, from_sequence, limit, notice)))
}

fn metrics_query(
    view: MetricsView,
    bloom: Option<Vec<u8>>,
    from_sequence: Option<u64>,
    limit: Option<u64>,
    notice: Option<String>,
) -> MetricsQuery {
    MetricsQuery { view, bloom, from_sequence, limit, notice }
}

/// `GET /spend`
pub(super) fn spend() -> Routed {
    Routed::Spend(SpendQuery)
}

pub(super) fn metrics_response(result: MetricsQueryResult) -> HttpServerResponse {
    match result {
        MetricsQueryResult::Ok { document, notice } => {
            let mut response = render(&document);
            if let Some(notice) = notice {
                response.headers.push(HttpHeader { name: "x-aether-notice".to_owned(), value: notice });
            }
            response
        }
        MetricsQueryResult::NotFound => error_response(404, "no such bloom"),
        // Encode failures and incomplete reads are peer-cap errors, the same
        // class spend and projection reads answer `500`. Parse errors stay
        // `400` at the HTTP edge.
        MetricsQueryResult::Err { error } => error_response(500, &error),
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

type MetricsPage = (Option<u64>, Option<u64>, Option<String>);

fn parse_page(query: &str) -> Result<MetricsPage, String> {
    let mut from_sequence = None;
    let mut requested = None;
    for (key, value) in pairs(query) {
        match key.as_str() {
            "from_sequence" => from_sequence = Some(parse_u64("from_sequence", &value)?),
            "limit" => requested = Some(parse_u64("limit", &value)?),
            _ => {}
        }
    }
    let (limit, notice) = clamp_limit(requested, METRICS_DEFAULT_LIMIT, METRICS_MAX_LIMIT);
    Ok((from_sequence, Some(limit), notice))
}
