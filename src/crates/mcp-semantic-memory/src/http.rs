//! What the two HTTP facades share: a client, the address of an endpoint and
//! what counts as a failed request.

use std::time::Duration;

use reqwest::{Client, RequestBuilder, Response, Url};

use crate::types::BoxError;

/// A client that gives up on a request after `timeout`.
pub(crate) fn client(timeout: Duration) -> Result<Client, BoxError> {
  Client::builder()
    .timeout(timeout)
    .build()
    .map_err(BoxError::from)
}

/// The address of `path` on the server at `base`, which has no path of its
/// own, like `http://localhost:8080`.
pub(crate) fn endpoint(base: &str, path: &str) -> Result<Url, BoxError> {
  let address = format!("{}/{path}", base.trim_end_matches('/'));
  Url::parse(&address).map_err(BoxError::from)
}

/// Sends the request. A status that is not a success is a failure, and the
/// server's own words about it are kept, since they say what was wrong.
pub(crate) async fn send(
  request: RequestBuilder,
) -> Result<Response, BoxError> {
  let response = request.send().await?;
  let status = response.status();
  if status.is_success() {
    return Ok(response);
  }
  let body = response.text().await.unwrap_or_default();
  Err(format!("the server answered {status}: {body}").into())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_endpoint_is_the_path_on_the_base_with_or_without_a_slash() {
    for base in ["http://localhost:8080", "http://localhost:8080/"] {
      let url = endpoint(base, "v1/embeddings").unwrap();
      assert_eq!(url.as_str(), "http://localhost:8080/v1/embeddings");
    }
  }

  #[test]
  fn a_base_that_is_not_an_address_is_an_error() {
    assert!(endpoint("not an address", "v1/embeddings").is_err());
  }
}
