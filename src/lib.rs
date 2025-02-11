use log::{debug, warn};
use proxy_wasm::{
    traits::{Context, HttpContext, RootContext},
    types::{Action, LogLevel},
};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{cell::RefCell, collections::HashMap, error::Error, time::Duration};

const POWERED_BY: &str = "header-augmenting-filter";
const CACHE_KEY: &str = "cache";
const INITIALISATION_TICK: Duration = Duration::from_secs(3);

#[derive(Deserialize, Debug)]
#[serde(default)]
struct FilterConfig {
    /// The Envoy cluster name housing a HTTP service that will provide headers
    /// to add to requests.
    header_providing_service_cluster: String,

    /// The path to call on the HTTP service providing headers.
    header_providing_service_path: String,

    /// The authority to set when calling the HTTP service providing headers.
    header_providing_service_authority: String,

    /// The length of time to keep headers cached.
    #[serde(with = "serde_humanize_rs")]
    header_cache_expiry: Duration,
}

impl Default for FilterConfig {
    fn default() -> Self {
        FilterConfig {
            header_providing_service_cluster: "sidecar".to_owned(),
            header_providing_service_path: "/headers".to_owned(),
            header_providing_service_authority: "sidecar".to_owned(),
            header_cache_expiry: Duration::from_secs(360),
        }
    }
}

thread_local! {
    static CONFIGS: RefCell<HashMap<u32, FilterConfig>> = RefCell::new(HashMap::new())
}

#[no_mangle]
pub fn _start() {
    // set log level in start
    proxy_wasm::set_log_level(LogLevel::Trace);

    // root context - singleton
    // load a default FilterConfig in start
    proxy_wasm::set_root_context(|context_id| -> Box<dyn RootContext> {
        CONFIGS.with(|configs| {
            configs
                .borrow_mut()
                .insert(context_id, FilterConfig::default());
        });

        // Box is a smart pointer to a heap allocated value of type T
        Box::new(RootHandler { context_id })
    });

    // called during http filter chain
    proxy_wasm::set_http_context(|_context_id, _root_context_id| -> Box<dyn HttpContext> {
        Box::new(HttpHandler {})
    })
}

struct RootHandler {
    context_id: u32,
}

// remember this is a singleton and there is a hook for loading any configuration
// the tick is a poll action
impl RootContext for RootHandler {
    fn on_configure(&mut self, _config_size: usize) -> bool {
        // Check for the mandatory filter configuration stanza.
        let configuration: Vec<u8> = match self.get_configuration() {
            Some(c) => c,
            None => {
                warn!("configuration missing");

                return false;
            }
        };

        // Parse and store the configuration.
        match serde_json::from_slice::<FilterConfig>(configuration.as_ref()) {
            Ok(config) => {
                debug!("configuring {}: {:?}", self.context_id, config);
                CONFIGS.with(|configs| configs.borrow_mut().insert(self.context_id, config));
            }
            Err(e) => {
                warn!("failed to parse configuration: {:?}", e);

                return false;
            }
        }

        // Configure an initialisation tick and the cache.
        self.set_tick_period(INITIALISATION_TICK);
        self.set_shared_data(CACHE_KEY, None, None).is_ok()
    }

    // on_tick do the following actions every `tick_period`
    fn on_tick(&mut self) {
        // Log the action that is about to be taken.
        match self.get_shared_data(CACHE_KEY) {
            (None, _) => debug!("initialising cached headers"),
            (Some(_), _) => debug!("refreshing cached headers"),
        }

        // this gets called every `tick_period` and then dispatches an http call
        CONFIGS.with(|configs| {
            configs.borrow().get(&self.context_id).map(|config| {
                // We could be in the initialisation tick here so update our
                // tick period to the configured expiry before doing anything.
                // This will be reset to an initialisation tick upon failures.
                self.set_tick_period(config.header_cache_expiry);

                // Dispatch an async HTTP call to the configured cluster.
                // remember this is an async HTTP call
                // this is a trait of Context, RootContext extends it
                self.dispatch_http_call(
                    // this is what is getting the Authorization
                    &config.header_providing_service_cluster,
                    vec![
                        (":method", "GET"),
                        (":path", &config.header_providing_service_path),
                        (":authority", &config.header_providing_service_authority),
                    ],
                    None,
                    vec![],
                    Duration::from_secs(5),
                )
                .map_err(|e| {
                    // Something went wrong instantly. Reset to an
                    // initialisation tick for a quick retry.
                    self.set_tick_period(INITIALISATION_TICK);

                    warn!("failed calling header providing service: {:?}", e)
                })
            })
        });
    }
}

impl Context for RootHandler {
    fn on_http_call_response(
        &mut self,
        _token_id: u32,
        _num_headers: usize,
        body_size: usize,
        _num_trailers: usize,
    ) {
        // Gather the response body of previously dispatched async HTTP call.
        let body = match self.get_http_call_response_body(0, body_size) {
            Some(body) => body,
            None => {
                warn!("header providing service returned empty body");

                return;
            }
        };

        // Store the body in the shared cache.
        match self.set_shared_data(CACHE_KEY, Some(&body), None) {
            Ok(()) => debug!(
                "refreshed header cache with: {}",
                String::from_utf8(body.clone()).unwrap()
            ),

            Err(e) => {
                warn!("failed storing header cache: {:?}", e);

                // Reset to an initialisation tick for a quick retry.
                self.set_tick_period(INITIALISATION_TICK)
            }
        }
    }
}

struct HttpHandler {}

impl HttpContext for HttpHandler {
    fn on_http_request_headers(&mut self, _num_headers: usize) -> Action {
        match self.get_shared_data(CACHE_KEY) {
            (Some(cache), _) => {
                debug!(
                    "using existing header cache: {}",
                    String::from_utf8(cache.clone()).unwrap()
                );

                match self.parse_headers(&cache) {
                    Ok(headers) => {
                        for (name, value) in headers {
                            self.set_http_request_header(&name, value.as_str())
                        }
                    }
                    Err(e) => warn!("no usable headers cached: {:?}", e),
                }

                Action::Continue
            }
            (None, _) => {
                warn!("filter not initialised");

                self.send_http_response(
                    500,
                    vec![("Powered-By", POWERED_BY)],
                    Some(b"Filter not initialised"),
                );

                Action::Pause
            }
        }
    }
}

impl Context for HttpHandler {}

impl HttpHandler {
    fn parse_headers(&self, res: &[u8]) -> Result<Map<String, Value>, Box<dyn Error>> {
        Ok(serde_json::from_slice::<Value>(&res)?
            .as_object()
            .unwrap()
            .clone())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn todo() {
        assert_eq!(2 + 2, 4);
    }
}
