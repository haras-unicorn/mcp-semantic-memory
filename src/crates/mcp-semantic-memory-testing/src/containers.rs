//! Databases in containers, from images that Nix built.
//!
//! The dev shell says where each image is, with two environment variables per
//! image: the archive `dockerTools` made, and the name and tag inside it. The
//! tag comes from the contents, so an image that is already loaded is the
//! right one. Nothing is ever pulled.

#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  reason = "this fails a test by panicking, like the test would"
)]

use std::env;

use bollard::Docker;
use bollard::query_parameters::ImportImageOptionsBuilder;
use futures_util::StreamExt;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{
  ContainerAsync, ContainerRequest, GenericImage, ImageExt,
};
use tokio_util::io::ReaderStream;

/// How to start a database from an image.
#[derive(Debug, Clone, Copy)]
pub struct Spec<'a> {
  /// The environment variable that holds the path of the image archive.
  pub archive: &'a str,
  /// The environment variable that holds the name and the tag of the image,
  /// as `name:tag`.
  pub image: &'a str,
  /// The port the database listens on inside the container.
  pub port: u16,
  /// What the database says on stderr once it takes connections.
  pub ready: &'a str,
  /// The environment of the container.
  pub environment: &'a [(&'a str, &'a str)],
}

/// A database that is up. It goes away when this is dropped.
pub struct Container {
  /// Where the database is reachable from the test.
  pub host: String,
  /// The port the database is reachable on.
  pub port: u16,
  _container: ContainerAsync<GenericImage>,
}

/// Loads the image if the container runtime does not have it, and starts a
/// container from it. Waits until the database takes connections.
pub async fn start(spec: &Spec<'_>) -> Container {
  let archive = variable(spec.archive);
  let image = variable(spec.image);
  load(&archive, &image).await;

  let (name, tag) = image
    .rsplit_once(':')
    .unwrap_or_else(|| panic!("{} is not name:tag: {image}", spec.image));
  let mut request: ContainerRequest<GenericImage> =
    GenericImage::new(name, tag)
      .with_exposed_port(spec.port.tcp())
      .with_wait_for(WaitFor::message_on_stderr(spec.ready))
      .into();
  for (key, value) in spec.environment {
    request = request.with_env_var(*key, *value);
  }

  let container = request
    .start()
    .await
    .expect("could not start the container");
  Container {
    host: container.get_host().await.unwrap().to_string(),
    port: container.get_host_port_ipv4(spec.port.tcp()).await.unwrap(),
    _container: container,
  }
}

fn variable(name: &str) -> String {
  env::var(name).unwrap_or_else(|_| {
    panic!("{name} is not set. The tests run in the dev shell.")
  })
}

async fn load(archive: &str, image: &str) {
  let docker = Docker::connect_with_defaults()
    .expect("could not reach the container runtime (DOCKER_HOST)");
  if docker.inspect_image(image).await.is_ok() {
    return;
  }

  let file = tokio::fs::File::open(archive)
    .await
    .unwrap_or_else(|error| panic!("could not open {archive}: {error}"));
  let mut loading = docker.import_image_stream(
    ImportImageOptionsBuilder::default().build(),
    ReaderStream::new(file),
    None,
  );
  while let Some(step) = loading.next().await {
    step.expect("the container runtime could not load the image");
  }
}
