use super::*;

use std::sync::{Arc, Mutex};

use anyhow::Error;
use axum::{
    body::Body,
    http::Request,
    middleware,
    response::IntoResponse,
    routing::method_routing::{get, post},
    Router,
};
use ethnum::u256;
use ic_types::messages::{
    Blob, HttpCallContent, HttpCanisterUpdate, HttpQueryContent, HttpReadState,
    HttpReadStateContent, HttpRequestEnvelope, HttpUserQuery,
};
use prometheus::Registry;
use tower::{Service, ServiceBuilder};
use tower_http::{compression::CompressionLayer, request_id::MakeRequestUuid, ServiceBuilderExt};

use crate::{
    management::btc_mw,
    metrics::{
        metrics_middleware, metrics_middleware_status, HttpMetricParams, HttpMetricParamsStatus,
    },
    persist::test::node,
    retry::{retry_request, RetryParams},
};

#[derive(Clone)]
struct ProxyRouter {
    root_key: Vec<u8>,
    health: Arc<Mutex<ReplicaHealthStatus>>,
}

impl ProxyRouter {
    fn set_health(&self, new: ReplicaHealthStatus) {
        let mut h = self.health.lock().unwrap();
        *h = new;
    }
}

#[async_trait]
impl Proxy for ProxyRouter {
    async fn proxy(
        &self,
        request_type: RequestType,
        _request: Request<Body>,
        _node: Node,
        _canister_id: CanisterId,
    ) -> Result<Response, ErrorCause> {
        let mut resp = "test_response".into_response();

        let status = match request_type {
            RequestType::Call => StatusCode::ACCEPTED,
            _ => StatusCode::OK,
        };

        *resp.status_mut() = status;
        Ok(resp)
    }
}

pub fn test_node(id: u64) -> Node {
    node(id, Principal::from_text("f7crg-kabae").unwrap())
}

pub fn test_route_subnet(n: usize) -> RouteSubnet {
    let mut nodes = Vec::new();

    for i in 0..n {
        nodes.push(test_node(i as u64));
    }

    // "casting integer literal to `u32` is unnecessary"
    // fck clippy
    let zero = 0u32;

    RouteSubnet {
        id: Principal::from_text("f7crg-kabae").unwrap().to_string(),
        range_start: u256::from(zero),
        range_end: u256::from(zero),
        nodes,
    }
}

#[async_trait]
impl Lookup for ProxyRouter {
    async fn lookup_subnet(&self, _: &CanisterId) -> Result<Arc<RouteSubnet>, ErrorCause> {
        Ok(Arc::new(test_route_subnet(1)))
    }
}

#[async_trait]
impl RootKey for ProxyRouter {
    async fn root_key(&self) -> Option<Vec<u8>> {
        Some(self.root_key.clone())
    }
}

#[async_trait]
impl Health for ProxyRouter {
    async fn health(&self) -> ReplicaHealthStatus {
        *self.health.lock().unwrap()
    }
}

#[tokio::test]
async fn test_middleware_validate_request() -> Result<(), Error> {
    let root_key = vec![8, 6, 7, 5, 3, 0, 9];

    let proxy_router = Arc::new(ProxyRouter {
        root_key: root_key.clone(),
        health: Arc::new(Mutex::new(ReplicaHealthStatus::Healthy)),
    });

    let (state_rootkey, state_health) = (
        proxy_router.clone() as Arc<dyn RootKey>,
        proxy_router.clone() as Arc<dyn Health>,
    );

    // NOTE: this router should be aligned with the one in core.rs, otherwise this testing is useless.
    let mut app = Router::new()
        .route(
            PATH_STATUS,
            get(status).with_state((state_rootkey, state_health)),
        )
        .layer(
            ServiceBuilder::new()
                .layer(middleware::from_fn(validate_request))
                .set_x_request_id(MakeRequestUuid)
                .propagate_x_request_id(),
        );

    // case 1: no 'x-request-id' header, middleware generates one with a random uuid
    let request = Request::builder()
        .method("GET")
        .uri("http://localhost/api/v2/status")
        .body(Body::from(""))
        .unwrap();
    let resp = app.call(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let request_id = resp
        .headers()
        .get(HEADER_X_REQUEST_ID)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(UUID_REGEX.is_match(request_id));

    // case 2: 'x-request-id' header contains a valid uuid, this uuid is not overwritten by middleware
    let request = Request::builder()
        .method("GET")
        .uri("http://localhost/api/v2/status")
        .header(HEADER_X_REQUEST_ID, "40a6d613-149e-4bde-8443-33593fd2fd17")
        .body(Body::from(""))
        .unwrap();
    let resp = app.call(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(HEADER_X_REQUEST_ID).unwrap(),
        "40a6d613-149e-4bde-8443-33593fd2fd17"
    );
    // case 3: 'x-request-id' header contains an invalid uuid
    #[allow(clippy::borrow_interior_mutable_const)]
    let expected_failure = format!(
        "malformed_request: value of '{HEADER_X_REQUEST_ID}' header is not in UUID format\n"
    );
    let request = Request::builder()
        .method("GET")
        .uri("http://localhost/api/v2/status")
        .header(HEADER_X_REQUEST_ID, "1")
        .body(Body::from(""))
        .unwrap();
    let resp = app.call(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = hyper::body::to_bytes(resp).await.unwrap().to_vec();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(body, expected_failure);
    // case 4: 'x-request-id' header contains an invalid (not hyphenated) uuid
    let request = Request::builder()
        .method("GET")
        .uri("http://localhost/api/v2/status")
        .header(HEADER_X_REQUEST_ID, "40a6d613149e4bde844333593fd2fd17")
        .body(Body::from(""))
        .unwrap();
    let resp = app.call(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = hyper::body::to_bytes(resp).await.unwrap().to_vec();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(body, expected_failure);
    // case 5: 'x-request-id' header is empty
    let request = Request::builder()
        .method("GET")
        .uri("http://localhost/api/v2/status")
        .header(HEADER_X_REQUEST_ID, "")
        .body(Body::from(""))
        .unwrap();
    let resp = app.call(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = hyper::body::to_bytes(resp).await.unwrap().to_vec();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(body, expected_failure);

    Ok(())
}

#[tokio::test]
async fn test_health() -> Result<(), Error> {
    let root_key = vec![8, 6, 7, 5, 3, 0, 9];

    let proxy_router = Arc::new(ProxyRouter {
        root_key: root_key.clone(),
        health: Arc::new(Mutex::new(ReplicaHealthStatus::Healthy)),
    });

    let state_health = proxy_router.clone() as Arc<dyn Health>;
    let mut app = Router::new().route(PATH_HEALTH, get(health).with_state(state_health));

    // Test healthy
    let request = Request::builder()
        .method("GET")
        .uri("http://localhost/health")
        .body(Body::from(""))
        .unwrap();

    let resp = app.call(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Test starting
    proxy_router.set_health(ReplicaHealthStatus::Starting);

    let request = Request::builder()
        .method("GET")
        .uri("http://localhost/health")
        .body(Body::from(""))
        .unwrap();

    let resp = app.call(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

    Ok(())
}

#[tokio::test]
async fn test_status() -> Result<(), Error> {
    let root_key = vec![8, 6, 7, 5, 3, 0, 9];

    let proxy_router = Arc::new(ProxyRouter {
        root_key: root_key.clone(),
        health: Arc::new(Mutex::new(ReplicaHealthStatus::Healthy)),
    });

    let (state_rootkey, state_health) = (
        proxy_router.clone() as Arc<dyn RootKey>,
        proxy_router.clone() as Arc<dyn Health>,
    );

    let registry: Registry = Registry::new_custom(None, None)?;
    let metric_params = HttpMetricParamsStatus::new(&registry);

    let mut app = Router::new()
        .route(
            PATH_STATUS,
            get(status).with_state((state_rootkey, state_health)),
        )
        .layer(middleware::from_fn_with_state(
            metric_params,
            metrics_middleware_status,
        ))
        .layer(middleware::from_fn(validate_request));

    // Test healthy
    let request = Request::builder()
        .method("GET")
        .uri("http://localhost/api/v2/status")
        .body(Body::from(""))
        .unwrap();

    let resp = app.call(request).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let (_parts, body) = resp.into_parts();
    let body = hyper::body::to_bytes(body).await.unwrap().to_vec();

    let health: HttpStatusResponse = serde_cbor::from_slice(&body)?;
    assert_eq!(
        health.replica_health_status,
        Some(ReplicaHealthStatus::Healthy)
    );
    assert_eq!(health.root_key.as_deref(), Some(&root_key),);

    // Test starting
    proxy_router.set_health(ReplicaHealthStatus::Starting);

    let request = Request::builder()
        .method("GET")
        .uri("http://localhost/api/v2/status")
        .body(Body::from(""))
        .unwrap();

    let resp = app.call(request).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let (_parts, body) = resp.into_parts();
    let body = hyper::body::to_bytes(body).await.unwrap().to_vec();

    let health: HttpStatusResponse = serde_cbor::from_slice(&body)?;
    assert_eq!(
        health.replica_health_status,
        Some(ReplicaHealthStatus::Starting)
    );

    Ok(())
}

#[tokio::test]
async fn test_all_call_types() -> Result<(), Error> {
    let node = test_node(0);
    let root_key = vec![8, 6, 7, 5, 3, 0, 9];
    let state = Arc::new(ProxyRouter {
        root_key,
        health: Arc::new(Mutex::new(ReplicaHealthStatus::Healthy)),
    });

    let (state_lookup, state_proxy) = (
        state.clone() as Arc<dyn Lookup>,
        state.clone() as Arc<dyn Proxy>,
    );

    let registry: Registry = Registry::new_custom(None, None)?;
    let metric_params = HttpMetricParams::new(&registry, "foo");

    let mut app = Router::new()
        .route(
            PATH_QUERY,
            post(handle_call).with_state(state_proxy.clone()),
        )
        .route(PATH_CALL, post(handle_call).with_state(state_proxy.clone()))
        .route(PATH_READ_STATE, post(handle_call).with_state(state_proxy))
        .layer(
            ServiceBuilder::new()
                .layer(middleware::from_fn(validate_request))
                .layer(middleware::from_fn(postprocess_response))
                .layer(CompressionLayer::new())
                .layer(middleware::from_fn(pre_compression))
                .set_x_request_id(MakeRequestUuid)
                .propagate_x_request_id()
                .layer(middleware::from_fn_with_state(
                    metric_params,
                    metrics_middleware,
                ))
                .layer(middleware::from_fn(preprocess_request))
                .layer(middleware::from_fn(btc_mw))
                .layer(middleware::from_fn_with_state(state_lookup, lookup_subnet))
                .layer(middleware::from_fn_with_state(
                    RetryParams {
                        retry_count: 1,
                        retry_update_call: false,
                    },
                    retry_request,
                )),
        );

    let sender = Principal::from_text("sqjm4-qahae-aq").unwrap();
    let canister_id = Principal::from_text("sxiki-5ygae-aq").unwrap();

    // Test query
    let content = HttpQueryContent::Query {
        query: HttpUserQuery {
            canister_id: Blob(canister_id.as_slice().to_vec()),
            method_name: "foobar".to_string(),
            arg: Blob(vec![]),
            sender: Blob(sender.as_slice().to_vec()),
            nonce: None,
            ingress_expiry: 1234,
        },
    };

    let envelope = HttpRequestEnvelope::<HttpQueryContent> {
        content,
        sender_delegation: None,
        sender_pubkey: None,
        sender_sig: None,
    };

    let body = serde_cbor::to_vec(&envelope).unwrap();

    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://localhost/api/v2/canister/{canister_id}/query"
        ))
        .body(Body::from(body))
        .unwrap();

    let resp = app.call(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Check response headers
    assert_eq!(
        resp.headers()
            .get(HEADER_IC_NODE_ID)
            .unwrap()
            .to_str()
            .unwrap(),
        node.id.to_string()
    );

    assert_eq!(
        resp.headers()
            .get(HEADER_IC_SUBNET_ID)
            .unwrap()
            .to_str()
            .unwrap(),
        node.subnet_id.to_string()
    );

    assert_eq!(
        resp.headers()
            .get(HEADER_IC_SUBNET_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        node.subnet_type.as_ref()
    );

    assert_eq!(
        resp.headers()
            .get(HEADER_IC_SENDER)
            .unwrap()
            .to_str()
            .unwrap(),
        sender.to_string(),
    );

    assert_eq!(
        resp.headers()
            .get(HEADER_IC_CANISTER_ID)
            .unwrap()
            .to_str()
            .unwrap(),
        canister_id.to_string(),
    );

    assert_eq!(
        resp.headers()
            .get(HEADER_IC_METHOD_NAME)
            .unwrap()
            .to_str()
            .unwrap(),
        "foobar",
    );

    assert_eq!(
        resp.headers()
            .get(HEADER_IC_REQUEST_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "query",
    );

    let (_parts, body) = resp.into_parts();
    let body = hyper::body::to_bytes(body).await.unwrap().to_vec();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(body, "test_response");

    // Test call
    let content = HttpCallContent::Call {
        update: HttpCanisterUpdate {
            canister_id: Blob(canister_id.as_slice().to_vec()),
            method_name: "foobar".to_string(),
            arg: Blob(vec![]),
            sender: Blob(sender.as_slice().to_vec()),
            nonce: None,
            ingress_expiry: 1234,
        },
    };

    let envelope = HttpRequestEnvelope::<HttpCallContent> {
        content,
        sender_delegation: None,
        sender_pubkey: None,
        sender_sig: None,
    };

    let body = serde_cbor::to_vec(&envelope).unwrap();

    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://localhost/api/v2/canister/{canister_id}/call"
        ))
        .body(Body::from(body))
        .unwrap();

    let resp = app.call(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let (_parts, body) = resp.into_parts();
    let body = hyper::body::to_bytes(body).await.unwrap().to_vec();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(body, "test_response");

    // Test read_state
    let content = HttpReadStateContent::ReadState {
        read_state: HttpReadState {
            sender: Blob(sender.as_slice().to_vec()),
            nonce: None,
            ingress_expiry: 1234,
            paths: vec![],
        },
    };

    let envelope = HttpRequestEnvelope::<HttpReadStateContent> {
        content,
        sender_delegation: None,
        sender_pubkey: None,
        sender_sig: None,
    };

    let body = serde_cbor::to_vec(&envelope).unwrap();

    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://localhost/api/v2/canister/{canister_id}/read_state"
        ))
        .body(Body::from(body))
        .unwrap();

    let resp = app.call(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let (_parts, body) = resp.into_parts();
    let body = hyper::body::to_bytes(body).await.unwrap().to_vec();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(body, "test_response");

    Ok(())
}
