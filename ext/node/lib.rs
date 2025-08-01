// Copyright 2018-2025 the Deno authors. MIT license.

#![deny(clippy::print_stderr)]
#![deny(clippy::print_stdout)]
#![allow(clippy::too_many_arguments)]

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::Path;

use deno_core::FastString;
use deno_core::OpState;
use deno_core::op2;
use deno_core::url::Url;
#[allow(unused_imports)]
use deno_core::v8;
use deno_core::v8::ExternalReference;
use deno_error::JsErrorBox;
use deno_permissions::CheckedPath;
use deno_permissions::OpenAccessKind;
use deno_permissions::PermissionsContainer;
use node_resolver::DenoIsBuiltInNodeModuleChecker;
use node_resolver::InNpmPackageChecker;
use node_resolver::IsBuiltInNodeModuleChecker;
use node_resolver::NpmPackageFolderResolver;
use node_resolver::PackageJsonResolverRc;
use node_resolver::errors::ClosestPkgJsonError;
use once_cell::sync::Lazy;

extern crate libz_sys as zlib;

mod global;
pub mod ops;

pub use deno_package_json::PackageJson;
use deno_permissions::PermissionCheckError;
pub use node_resolver::DENO_SUPPORTED_BUILTIN_NODE_MODULES as SUPPORTED_BUILTIN_NODE_MODULES;
pub use node_resolver::PathClean;
use ops::handle_wrap::AsyncId;
pub use ops::ipc::ChildPipeFd;
use ops::vm;
pub use ops::vm::ContextInitMode;
pub use ops::vm::VM_CONTEXT_INDEX;
pub use ops::vm::create_v8_context;
pub use ops::vm::init_global_template;

pub use crate::global::GlobalsStorage;
use crate::global::global_object_middleware;
use crate::global::global_template_middleware;

pub fn is_builtin_node_module(module_name: &str) -> bool {
  DenoIsBuiltInNodeModuleChecker.is_builtin_node_module(module_name)
}

pub trait NodePermissions {
  fn check_net_url(
    &mut self,
    url: &Url,
    api_name: &str,
  ) -> Result<(), PermissionCheckError>;
  fn check_net(
    &mut self,
    host: (&str, Option<u16>),
    api_name: &str,
  ) -> Result<(), PermissionCheckError>;
  #[must_use = "the resolved return value to mitigate time-of-check to time-of-use issues"]
  fn check_open<'a>(
    &mut self,
    path: Cow<'a, Path>,
    open_access: OpenAccessKind,
    api_name: Option<&str>,
  ) -> Result<CheckedPath<'a>, PermissionCheckError>;
  fn query_read_all(&mut self) -> bool;
  fn check_sys(
    &mut self,
    kind: &str,
    api_name: &str,
  ) -> Result<(), PermissionCheckError>;
}

impl NodePermissions for deno_permissions::PermissionsContainer {
  #[inline(always)]
  fn check_net_url(
    &mut self,
    url: &Url,
    api_name: &str,
  ) -> Result<(), PermissionCheckError> {
    deno_permissions::PermissionsContainer::check_net_url(self, url, api_name)
  }

  fn check_net(
    &mut self,
    host: (&str, Option<u16>),
    api_name: &str,
  ) -> Result<(), PermissionCheckError> {
    deno_permissions::PermissionsContainer::check_net(self, &host, api_name)
  }

  fn check_open<'a>(
    &mut self,
    path: Cow<'a, Path>,
    open_access: OpenAccessKind,
    api_name: Option<&str>,
  ) -> Result<CheckedPath<'a>, PermissionCheckError> {
    deno_permissions::PermissionsContainer::check_open(
      self,
      path,
      open_access,
      api_name,
    )
  }

  fn query_read_all(&mut self) -> bool {
    deno_permissions::PermissionsContainer::query_read_all(self)
  }

  fn check_sys(
    &mut self,
    kind: &str,
    api_name: &str,
  ) -> Result<(), PermissionCheckError> {
    deno_permissions::PermissionsContainer::check_sys(self, kind, api_name)
  }
}

#[allow(clippy::disallowed_types)]
pub type NodeRequireLoaderRc = std::rc::Rc<dyn NodeRequireLoader>;

pub trait NodeRequireLoader {
  #[must_use = "the resolved return value to mitigate time-of-check to time-of-use issues"]
  fn ensure_read_permission<'a>(
    &self,
    permissions: &mut dyn NodePermissions,
    path: Cow<'a, Path>,
  ) -> Result<Cow<'a, Path>, JsErrorBox>;

  fn load_text_file_lossy(&self, path: &Path)
  -> Result<FastString, JsErrorBox>;

  /// Get if the module kind is maybe CJS and loading should determine
  /// if its CJS or ESM.
  fn is_maybe_cjs(&self, specifier: &Url) -> Result<bool, ClosestPkgJsonError>;

  fn resolve_require_node_module_paths(&self, from: &Path) -> Vec<String> {
    default_resolve_require_node_module_paths(from)
  }
}

pub fn default_resolve_require_node_module_paths(from: &Path) -> Vec<String> {
  let mut paths = Vec::with_capacity(from.components().count());
  let mut current_path = from;
  let mut maybe_parent = Some(current_path);
  while let Some(parent) = maybe_parent {
    if !parent.ends_with("node_modules") {
      paths.push(parent.join("node_modules").to_string_lossy().into_owned());
    }
    current_path = parent;
    maybe_parent = current_path.parent();
  }

  paths
}

pub static NODE_ENV_VAR_ALLOWLIST: Lazy<HashSet<String>> = Lazy::new(|| {
  // The full list of environment variables supported by Node.js is available
  // at https://nodejs.org/api/cli.html#environment-variables
  let mut set = HashSet::new();
  set.insert("NODE_DEBUG".to_string());
  set.insert("NODE_OPTIONS".to_string());
  set
});

#[op2]
#[string]
fn op_node_build_os() -> String {
  env!("TARGET").split('-').nth(2).unwrap().to_string()
}

#[derive(Debug, thiserror::Error, deno_error::JsError)]
enum DotEnvLoadErr {
  #[class(generic)]
  #[error(transparent)]
  DotEnv(#[from] dotenvy::Error),
  #[class(inherit)]
  #[error(transparent)]
  Permission(
    #[from]
    #[inherit]
    PermissionCheckError,
  ),
}

#[op2(fast)]
#[undefined]
fn op_node_load_env_file(
  state: &mut OpState,
  #[string] path: &str,
) -> Result<(), DotEnvLoadErr> {
  let path = state
    .borrow::<PermissionsContainer>()
    .check_open(
      Cow::Borrowed(Path::new(path)),
      OpenAccessKind::ReadNoFollow,
      Some("process.loadEnvFile"),
    )
    .map_err(DotEnvLoadErr::Permission)?;

  dotenvy::from_filename(path).map_err(DotEnvLoadErr::DotEnv)?;

  Ok(())
}

#[derive(Clone)]
pub struct NodeExtInitServices<
  TInNpmPackageChecker: InNpmPackageChecker,
  TNpmPackageFolderResolver: NpmPackageFolderResolver,
  TSys: ExtNodeSys,
> {
  pub node_require_loader: NodeRequireLoaderRc,
  pub node_resolver:
    NodeResolverRc<TInNpmPackageChecker, TNpmPackageFolderResolver, TSys>,
  pub pkg_json_resolver: PackageJsonResolverRc<TSys>,
  pub sys: TSys,
}

deno_core::extension!(deno_node,
  deps = [ deno_io, deno_fs ],
  parameters = [P: NodePermissions, TInNpmPackageChecker: InNpmPackageChecker, TNpmPackageFolderResolver: NpmPackageFolderResolver, TSys: ExtNodeSys],
  ops = [
    ops::blocklist::op_socket_address_parse,
    ops::blocklist::op_socket_address_get_serialization,

    ops::blocklist::op_blocklist_new,
    ops::blocklist::op_blocklist_add_address,
    ops::blocklist::op_blocklist_add_range,
    ops::blocklist::op_blocklist_add_subnet,
    ops::blocklist::op_blocklist_check,

    ops::buffer::op_is_ascii,
    ops::buffer::op_is_utf8,
    ops::buffer::op_transcode,
    ops::buffer::op_node_decode_utf8,
    ops::crypto::op_node_check_prime_async,
    ops::crypto::op_node_check_prime_bytes_async,
    ops::crypto::op_node_check_prime_bytes,
    ops::crypto::op_node_check_prime,
    ops::crypto::op_node_cipheriv_encrypt,
    ops::crypto::op_node_cipheriv_final,
    ops::crypto::op_node_cipheriv_set_aad,
    ops::crypto::op_node_cipheriv_take,
    ops::crypto::op_node_create_cipheriv,
    ops::crypto::op_node_create_decipheriv,
    ops::crypto::op_node_create_hash,
    ops::crypto::op_node_decipheriv_decrypt,
    ops::crypto::op_node_decipheriv_final,
    ops::crypto::op_node_decipheriv_set_aad,
    ops::crypto::op_node_decipheriv_auth_tag,
    ops::crypto::op_node_dh_compute_secret,
    ops::crypto::op_node_diffie_hellman,
    ops::crypto::op_node_ecdh_compute_public_key,
    ops::crypto::op_node_ecdh_compute_secret,
    ops::crypto::op_node_ecdh_encode_pubkey,
    ops::crypto::op_node_ecdh_generate_keys,
    ops::crypto::op_node_fill_random_async,
    ops::crypto::op_node_fill_random,
    ops::crypto::op_node_gen_prime_async,
    ops::crypto::op_node_gen_prime,
    ops::crypto::op_node_get_hash_size,
    ops::crypto::op_node_get_hashes,
    ops::crypto::op_node_hash_clone,
    ops::crypto::op_node_hash_digest_hex,
    ops::crypto::op_node_hash_digest,
    ops::crypto::op_node_hash_update_str,
    ops::crypto::op_node_hash_update,
    ops::crypto::op_node_hkdf_async,
    ops::crypto::op_node_hkdf,
    ops::crypto::op_node_pbkdf2_async,
    ops::crypto::op_node_pbkdf2,
    ops::crypto::op_node_pbkdf2_validate,
    ops::crypto::op_node_private_decrypt,
    ops::crypto::op_node_private_encrypt,
    ops::crypto::op_node_public_encrypt,
    ops::crypto::op_node_random_int,
    ops::crypto::op_node_scrypt_async,
    ops::crypto::op_node_scrypt_sync,
    ops::crypto::op_node_sign,
    ops::crypto::op_node_sign_ed25519,
    ops::crypto::op_node_verify,
    ops::crypto::op_node_verify_ed25519,
    ops::crypto::op_node_verify_spkac,
    ops::crypto::op_node_cert_export_public_key,
    ops::crypto::op_node_cert_export_challenge,
    ops::crypto::keys::op_node_create_private_key,
    ops::crypto::keys::op_node_create_ed_raw,
    ops::crypto::keys::op_node_create_rsa_jwk,
    ops::crypto::keys::op_node_create_ec_jwk,
    ops::crypto::keys::op_node_create_public_key,
    ops::crypto::keys::op_node_create_secret_key,
    ops::crypto::keys::op_node_derive_public_key_from_private_key,
    ops::crypto::keys::op_node_dh_keys_generate_and_export,
    ops::crypto::keys::op_node_export_private_key_der,
    ops::crypto::keys::op_node_export_private_key_jwk,
    ops::crypto::keys::op_node_export_private_key_pem,
    ops::crypto::keys::op_node_export_public_key_der,
    ops::crypto::keys::op_node_export_public_key_pem,
    ops::crypto::keys::op_node_export_public_key_jwk,
    ops::crypto::keys::op_node_export_secret_key_b64url,
    ops::crypto::keys::op_node_export_secret_key,
    ops::crypto::keys::op_node_generate_dh_group_key_async,
    ops::crypto::keys::op_node_generate_dh_group_key,
    ops::crypto::keys::op_node_generate_dh_key_async,
    ops::crypto::keys::op_node_generate_dh_key,
    ops::crypto::keys::op_node_generate_dsa_key_async,
    ops::crypto::keys::op_node_generate_dsa_key,
    ops::crypto::keys::op_node_generate_ec_key_async,
    ops::crypto::keys::op_node_generate_ec_key,
    ops::crypto::keys::op_node_generate_ed25519_key_async,
    ops::crypto::keys::op_node_generate_ed25519_key,
    ops::crypto::keys::op_node_generate_rsa_key_async,
    ops::crypto::keys::op_node_generate_rsa_key,
    ops::crypto::keys::op_node_generate_rsa_pss_key,
    ops::crypto::keys::op_node_generate_rsa_pss_key_async,
    ops::crypto::keys::op_node_generate_secret_key_async,
    ops::crypto::keys::op_node_generate_secret_key,
    ops::crypto::keys::op_node_generate_x25519_key_async,
    ops::crypto::keys::op_node_generate_x25519_key,
    ops::crypto::keys::op_node_get_asymmetric_key_details,
    ops::crypto::keys::op_node_get_asymmetric_key_type,
    ops::crypto::keys::op_node_get_private_key_from_pair,
    ops::crypto::keys::op_node_get_public_key_from_pair,
    ops::crypto::keys::op_node_get_symmetric_key_size,
    ops::crypto::keys::op_node_key_type,
    ops::crypto::x509::op_node_x509_parse,
    ops::crypto::x509::op_node_x509_ca,
    ops::crypto::x509::op_node_x509_check_email,
    ops::crypto::x509::op_node_x509_check_host,
    ops::crypto::x509::op_node_x509_fingerprint,
    ops::crypto::x509::op_node_x509_fingerprint256,
    ops::crypto::x509::op_node_x509_fingerprint512,
    ops::crypto::x509::op_node_x509_get_issuer,
    ops::crypto::x509::op_node_x509_get_subject,
    ops::crypto::x509::op_node_x509_get_valid_from,
    ops::crypto::x509::op_node_x509_get_valid_to,
    ops::crypto::x509::op_node_x509_get_serial_number,
    ops::crypto::x509::op_node_x509_key_usage,
    ops::crypto::x509::op_node_x509_public_key,
    ops::dns::op_node_getaddrinfo<P>,
    ops::fs::op_node_fs_exists_sync<P>,
    ops::fs::op_node_fs_exists<P>,
    ops::fs::op_node_cp_sync<P>,
    ops::fs::op_node_cp<P>,
    ops::fs::op_node_lchmod_sync<P>,
    ops::fs::op_node_lchmod<P>,
    ops::fs::op_node_lchown_sync<P>,
    ops::fs::op_node_lchown<P>,
    ops::fs::op_node_lutimes_sync<P>,
    ops::fs::op_node_lutimes<P>,
    ops::fs::op_node_statfs<P>,
    ops::winerror::op_node_sys_to_uv_error,
    ops::v8::op_v8_cached_data_version_tag,
    ops::v8::op_v8_get_heap_statistics,
    ops::v8::op_v8_get_wire_format_version,
    ops::v8::op_v8_new_deserializer,
    ops::v8::op_v8_new_serializer,
    ops::v8::op_v8_read_double,
    ops::v8::op_v8_read_header,
    ops::v8::op_v8_read_raw_bytes,
    ops::v8::op_v8_read_uint32,
    ops::v8::op_v8_read_uint64,
    ops::v8::op_v8_read_value,
    ops::v8::op_v8_release_buffer,
    ops::v8::op_v8_set_treat_array_buffer_views_as_host_objects,
    ops::v8::op_v8_transfer_array_buffer,
    ops::v8::op_v8_transfer_array_buffer_de,
    ops::v8::op_v8_write_double,
    ops::v8::op_v8_write_header,
    ops::v8::op_v8_write_raw_bytes,
    ops::v8::op_v8_write_uint32,
    ops::v8::op_v8_write_uint64,
    ops::v8::op_v8_write_value,
    ops::vm::op_vm_create_script,
    ops::vm::op_vm_create_context,
    ops::vm::op_vm_script_run_in_context,
    ops::vm::op_vm_is_context,
    ops::vm::op_vm_compile_function,
    ops::vm::op_vm_script_get_source_map_url,
    ops::vm::op_vm_script_create_cached_data,
    ops::idna::op_node_idna_domain_to_ascii,
    ops::idna::op_node_idna_domain_to_unicode,
    ops::idna::op_node_idna_punycode_to_ascii,
    ops::idna::op_node_idna_punycode_to_unicode,
    ops::idna::op_node_idna_punycode_decode,
    ops::idna::op_node_idna_punycode_encode,
    ops::zlib::op_zlib_crc32,
    ops::zlib::op_zlib_crc32_string,
    ops::handle_wrap::op_node_new_async_id,
    ops::http::op_node_http_fetch_response_upgrade,
    ops::http::op_node_http_request_with_conn<P>,
    ops::http::op_node_http_await_information,
    ops::http::op_node_http_await_response,
    ops::http2::op_http2_connect,
    ops::http2::op_http2_poll_client_connection,
    ops::http2::op_http2_client_request,
    ops::http2::op_http2_client_get_response,
    ops::http2::op_http2_client_get_response_body_chunk,
    ops::http2::op_http2_client_send_data,
    ops::http2::op_http2_client_reset_stream,
    ops::http2::op_http2_client_send_trailers,
    ops::http2::op_http2_client_get_response_trailers,
    ops::http2::op_http2_accept,
    ops::http2::op_http2_listen,
    ops::http2::op_http2_send_response,
    ops::os::op_node_os_get_priority<P>,
    ops::os::op_node_os_set_priority<P>,
    ops::os::op_node_os_user_info<P>,
    ops::os::op_geteuid<P>,
    ops::os::op_getegid<P>,
    ops::os::op_cpus<P>,
    ops::os::op_homedir<P>,
    op_node_build_os,
    op_node_load_env_file,
    ops::require::op_require_can_parse_as_esm,
    ops::require::op_require_init_paths,
    ops::require::op_require_node_module_paths<P, TSys>,
    ops::require::op_require_proxy_path,
    ops::require::op_require_is_deno_dir_package<TInNpmPackageChecker, TNpmPackageFolderResolver, TSys>,
    ops::require::op_require_resolve_deno_dir<TInNpmPackageChecker, TNpmPackageFolderResolver, TSys>,
    ops::require::op_require_is_maybe_cjs,
    ops::require::op_require_is_request_relative,
    ops::require::op_require_resolve_lookup_paths,
    ops::require::op_require_try_self<P, TInNpmPackageChecker, TNpmPackageFolderResolver, TSys>,
    ops::require::op_require_real_path<P, TSys>,
    ops::require::op_require_path_is_absolute,
    ops::require::op_require_path_dirname,
    ops::require::op_require_stat<P, TSys>,
    ops::require::op_require_path_resolve,
    ops::require::op_require_path_basename,
    ops::require::op_require_read_file<P>,
    ops::require::op_require_as_file_path,
    ops::require::op_require_resolve_exports<P, TInNpmPackageChecker, TNpmPackageFolderResolver, TSys>,
    ops::require::op_require_read_package_scope<P, TSys>,
    ops::require::op_require_package_imports_resolve<P, TInNpmPackageChecker, TNpmPackageFolderResolver, TSys>,
    ops::require::op_require_break_on_next_statement,
    ops::util::op_node_guess_handle_type,
    ops::util::op_node_view_has_buffer,
    ops::util::op_node_call_is_from_dependency<TInNpmPackageChecker, TNpmPackageFolderResolver, TSys>,
    ops::worker_threads::op_worker_threads_filename<P, TSys>,
    ops::ipc::op_node_child_ipc_pipe,
    ops::ipc::op_node_ipc_write,
    ops::ipc::op_node_ipc_read,
    ops::ipc::op_node_ipc_ref,
    ops::ipc::op_node_ipc_unref,
    ops::process::op_node_process_kill,
    ops::process::op_process_abort,
    ops::tls::op_get_root_certificates,
    ops::tls::op_tls_peer_certificate,
    ops::tls::op_tls_canonicalize_ipv4_address,
    ops::inspector::op_inspector_open<P>,
    ops::inspector::op_inspector_close,
    ops::inspector::op_inspector_url,
    ops::inspector::op_inspector_wait,
    ops::inspector::op_inspector_connect<P>,
    ops::inspector::op_inspector_dispatch,
    ops::inspector::op_inspector_disconnect,
    ops::inspector::op_inspector_emit_protocol_event,
    ops::inspector::op_inspector_enabled,
  ],
  objects = [
    ops::perf_hooks::EldHistogram,
    ops::sqlite::DatabaseSync,
    ops::sqlite::Session,
    ops::handle_wrap::AsyncWrap,
    ops::handle_wrap::HandleWrap,
    ops::sqlite::StatementSync,
    ops::crypto::digest::Hasher,
    ops::zlib::BrotliDecoder,
    ops::zlib::BrotliEncoder,
    ops::zlib::Zlib,
  ],
  esm_entry_point = "ext:deno_node/02_init.js",
  esm = [
    dir "polyfills",
    "00_globals.js",
    "02_init.js",
    "_events.mjs",
    "_fs/_fs_access.ts",
    "_fs/_fs_appendFile.ts",
    "_fs/_fs_chmod.ts",
    "_fs/_fs_chown.ts",
    "_fs/_fs_close.ts",
    "_fs/_fs_common.ts",
    "_fs/_fs_constants.ts",
    "_fs/_fs_copy.ts",
    "_fs/_fs_cp.js",
    "_fs/_fs_dir.ts",
    "_fs/_fs_exists.ts",
    "_fs/_fs_fchmod.ts",
    "_fs/_fs_fchown.ts",
    "_fs/_fs_fdatasync.ts",
    "_fs/_fs_fstat.ts",
    "_fs/_fs_fsync.ts",
    "_fs/_fs_ftruncate.ts",
    "_fs/_fs_futimes.ts",
    "_fs/_fs_glob.ts",
    "_fs/_fs_lchmod.ts",
    "_fs/_fs_lchown.ts",
    "_fs/_fs_link.ts",
    "_fs/_fs_lstat.ts",
    "_fs/_fs_lutimes.ts",
    "_fs/_fs_mkdir.ts",
    "_fs/_fs_mkdtemp.ts",
    "_fs/_fs_open.ts",
    "_fs/_fs_opendir.ts",
    "_fs/_fs_read.ts",
    "_fs/_fs_readdir.ts",
    "_fs/_fs_readFile.ts",
    "_fs/_fs_readlink.ts",
    "_fs/_fs_readv.ts",
    "_fs/_fs_realpath.ts",
    "_fs/_fs_rename.ts",
    "_fs/_fs_rm.ts",
    "_fs/_fs_rmdir.ts",
    "_fs/_fs_stat.ts",
    "_fs/_fs_statfs.js",
    "_fs/_fs_symlink.ts",
    "_fs/_fs_truncate.ts",
    "_fs/_fs_unlink.ts",
    "_fs/_fs_utimes.ts",
    "_fs/_fs_watch.ts",
    "_fs/_fs_write.mjs",
    "_fs/_fs_writeFile.ts",
    "_fs/_fs_writev.ts",
    "_next_tick.ts",
    "_process/exiting.ts",
    "_process/process.ts",
    "_process/streams.mjs",
    "_readline.mjs",
    "_util/_util_callbackify.js",
    "_util/asserts.ts",
    "_util/async.ts",
    "_util/os.ts",
    "_util/std_asserts.ts",
    "_util/std_fmt_colors.ts",
    "_util/std_testing_diff.ts",
    "_utils.ts",
    "_zlib_binding.mjs",
    "assertion_error.ts",
    "internal_binding/_libuv_winerror.ts",
    "internal_binding/_listen.ts",
    "internal_binding/_node.ts",
    "internal_binding/_timingSafeEqual.ts",
    "internal_binding/_utils.ts",
    "internal_binding/ares.ts",
    "internal_binding/async_wrap.ts",
    "internal_binding/buffer.ts",
    "internal_binding/cares_wrap.ts",
    "internal_binding/connection_wrap.ts",
    "internal_binding/constants.ts",
    "internal_binding/crypto.ts",
    "internal_binding/handle_wrap.ts",
    "internal_binding/http_parser.ts",
    "internal_binding/mod.ts",
    "internal_binding/node_file.ts",
    "internal_binding/node_options.ts",
    "internal_binding/pipe_wrap.ts",
    "internal_binding/stream_wrap.ts",
    "internal_binding/string_decoder.ts",
    "internal_binding/symbols.ts",
    "internal_binding/tcp_wrap.ts",
    "internal_binding/tty_wrap.ts",
    "internal_binding/types.ts",
    "internal_binding/udp_wrap.ts",
    "internal_binding/util.ts",
    "internal_binding/uv.ts",
    "internal/assert/calltracker.js",
    "internal/assert.mjs",
    "internal/async_hooks.ts",
    "internal/blocklist.mjs",
    "internal/buffer.mjs",
    "internal/child_process.ts",
    "internal/cli_table.ts",
    "internal/console/constructor.mjs",
    "internal/constants.ts",
    "internal/crypto/_keys.ts",
    "internal/crypto/_randomBytes.ts",
    "internal/crypto/_randomFill.mjs",
    "internal/crypto/_randomInt.ts",
    "internal/crypto/certificate.ts",
    "internal/crypto/cipher.ts",
    "internal/crypto/constants.ts",
    "internal/crypto/diffiehellman.ts",
    "internal/crypto/hash.ts",
    "internal/crypto/hkdf.ts",
    "internal/crypto/keygen.ts",
    "internal/crypto/keys.ts",
    "internal/crypto/pbkdf2.ts",
    "internal/crypto/random.ts",
    "internal/crypto/scrypt.ts",
    "internal/crypto/sig.ts",
    "internal/crypto/util.ts",
    "internal/crypto/x509.ts",
    "internal/dgram.ts",
    "internal/dns/promises.ts",
    "internal/dns/utils.ts",
    "internal/dtrace.ts",
    "internal/error_codes.ts",
    "internal/errors.ts",
    "internal/event_target.mjs",
    "internal/events/abort_listener.mjs",
    "internal/fixed_queue.ts",
    "internal/fs/streams.mjs",
    "internal/fs/utils.mjs",
    "internal/fs/handle.ts",
    "internal/hide_stack_frames.ts",
    "internal/http.ts",
    "internal/http2/util.ts",
    "internal/idna.ts",
    "internal/net.ts",
    "internal/normalize_encoding.mjs",
    "internal/options.ts",
    "internal/primordials.mjs",
    "internal/process/per_thread.mjs",
    "internal/process/report.ts",
    "internal/process/warning.ts",
    "internal/querystring.ts",
    "internal/readline/callbacks.mjs",
    "internal/readline/emitKeypressEvents.mjs",
    "internal/readline/interface.mjs",
    "internal/readline/promises.mjs",
    "internal/readline/symbols.mjs",
    "internal/readline/utils.mjs",
    "internal/stream_base_commons.ts",
    "internal/streams/add-abort-signal.js",
    "internal/streams/compose.js",
    "internal/streams/destroy.js",
    "internal/streams/duplexify.js",
    "internal/streams/duplexpair.js",
    "internal/streams/end-of-stream.js",
    "internal/streams/from.js",
    "internal/streams/lazy_transform.js",
    "internal/streams/legacy.js",
    "internal/streams/operators.js",
    "internal/streams/pipeline.js",
    "internal/streams/state.js",
    "internal/streams/utils.js",
    "internal/test/binding.ts",
    "internal/timers.mjs",
    "internal/url.ts",
    "internal/util.mjs",
    "internal/util/comparisons.ts",
    "internal/util/debuglog.ts",
    "internal/util/inspect.mjs",
    "internal/util/parse_args/parse_args.js",
    "internal/util/parse_args/utils.js",
    "internal/util/types.ts",
    "internal/validators.mjs",
    "internal/webstreams/adapters.js",
    "path/_constants.ts",
    "path/_interface.ts",
    "path/_util.ts",
    "path/_posix.ts",
    "path/_win32.ts",
    "path/common.ts",
    "path/mod.ts",
    "path/separator.ts",
    "readline/promises.ts",
    "node:_http_agent" = "_http_agent.mjs",
    "node:_http_common" = "_http_common.ts",
    "node:_http_outgoing" = "_http_outgoing.ts",
    "node:_http_server" = "_http_server.ts",
    "node:_stream_duplex" = "internal/streams/duplex.js",
    "node:_stream_passthrough" = "internal/streams/passthrough.js",
    "node:_stream_readable" = "internal/streams/readable.js",
    "node:_stream_transform" = "internal/streams/transform.js",
    "node:_stream_writable" = "internal/streams/writable.js",
    "node:_tls_common" = "_tls_common.ts",
    "node:_tls_wrap" = "_tls_wrap.ts",
    "node:assert" = "assert.ts",
    "node:assert/strict" = "assert/strict.ts",
    "node:async_hooks" = "async_hooks.ts",
    "node:buffer" = "buffer.ts",
    "node:child_process" = "child_process.ts",
    "node:cluster" = "cluster.ts",
    "node:console" = "console.ts",
    "node:constants" = "constants.ts",
    "node:crypto" = "crypto.ts",
    "node:dgram" = "dgram.ts",
    "node:diagnostics_channel" = "diagnostics_channel.js",
    "node:dns" = "dns.ts",
    "node:dns/promises" = "dns/promises.ts",
    "node:domain" = "domain.ts",
    "node:events" = "events.ts",
    "node:fs" = "fs.ts",
    "node:fs/promises" = "fs/promises.ts",
    "node:http" = "http.ts",
    "node:http2" = "http2.ts",
    "node:https" = "https.ts",
    "node:inspector" = "inspector.js",
    "node:inspector/promises" = "inspector/promises.js",
    "node:module" = "01_require.js",
    "node:net" = "net.ts",
    "node:os" = "os.ts",
    "node:path" = "path.ts",
    "node:path/posix" = "path/posix.ts",
    "node:path/win32" = "path/win32.ts",
    "node:perf_hooks" = "perf_hooks.js",
    "node:process" = "process.ts",
    "node:punycode" = "punycode.ts",
    "node:querystring" = "querystring.js",
    "node:readline" = "readline.ts",
    "node:readline/promises" = "readline/promises.ts",
    "node:repl" = "repl.ts",
    "node:sqlite" = "sqlite.ts",
    "node:stream" = "stream.ts",
    "node:stream/consumers" = "stream/consumers.js",
    "node:stream/promises" = "stream/promises.js",
    "node:stream/web" = "stream/web.js",
    "node:string_decoder" = "string_decoder.ts",
    "node:sys" = "sys.ts",
    "node:test" = "testing.ts",
    "node:timers" = "timers.ts",
    "node:timers/promises" = "timers/promises.ts",
    "node:tls" = "tls.ts",
    "node:trace_events" = "trace_events.ts",
    "node:tty" = "tty.js",
    "node:url" = "url.ts",
    "node:util" = "util.ts",
    "node:util/types" = "util/types.ts",
    "node:v8" = "v8.ts",
    "node:vm" = "vm.js",
    "node:wasi" = "wasi.ts",
    "node:worker_threads" = "worker_threads.ts",
    "node:zlib" = "zlib.js",
  ],
  lazy_loaded_esm = [
    dir "polyfills",
    "deps/minimatch.js",
  ],
  options = {
    maybe_init: Option<NodeExtInitServices<TInNpmPackageChecker, TNpmPackageFolderResolver, TSys>>,
    fs: deno_fs::FileSystemRc,
  },
  state = |state, options| {
    state.put(options.fs.clone());

    if let Some(init) = &options.maybe_init {
      state.put(init.sys.clone());
      state.put(init.node_require_loader.clone());
      state.put(init.node_resolver.clone());
      state.put(init.pkg_json_resolver.clone());
    }

    state.put(AsyncId::default());
  },
  global_template_middleware = global_template_middleware,
  global_object_middleware = global_object_middleware,
  customizer = |ext: &mut deno_core::Extension| {
    let external_references = [
      vm::QUERY_MAP_FN.with(|query| {
        ExternalReference {
          named_query: *query,
        }
      }),
      vm::GETTER_MAP_FN.with(|getter| {
        ExternalReference {
          named_getter: *getter,
        }
      }),
      vm::SETTER_MAP_FN.with(|setter| {
        ExternalReference {
          named_setter: *setter,
        }
      }),
      vm::DESCRIPTOR_MAP_FN.with(|descriptor| {
        ExternalReference {
          named_getter: *descriptor,
        }
      }),
      vm::DELETER_MAP_FN.with(|deleter| {
        ExternalReference {
          named_deleter: *deleter,
        }
      }),
      vm::ENUMERATOR_MAP_FN.with(|enumerator| {
        ExternalReference {
          enumerator: *enumerator,
        }
      }),
      vm::DEFINER_MAP_FN.with(|definer| {
        ExternalReference {
          named_definer: *definer,
        }
      }),

      vm::INDEXED_QUERY_MAP_FN.with(|query| {
        ExternalReference {
          indexed_query: *query,
        }
      }),
      vm::INDEXED_GETTER_MAP_FN.with(|getter| {
        ExternalReference {
          indexed_getter: *getter,
        }
      }),
      vm::INDEXED_SETTER_MAP_FN.with(|setter| {
        ExternalReference {
          indexed_setter: *setter,
        }
      }),
      vm::INDEXED_DESCRIPTOR_MAP_FN.with(|descriptor| {
        ExternalReference {
          indexed_getter: *descriptor,
        }
      }),
      vm::INDEXED_DELETER_MAP_FN.with(|deleter| {
        ExternalReference {
          indexed_deleter: *deleter,
        }
      }),
      vm::INDEXED_DEFINER_MAP_FN.with(|definer| {
        ExternalReference {
          indexed_definer: *definer,
        }
      }),
      vm::INDEXED_ENUMERATOR_MAP_FN.with(|enumerator| {
        ExternalReference {
          enumerator: *enumerator,
        }
      }),

      global::GETTER_MAP_FN.with(|getter| {
        ExternalReference {
          named_getter: *getter,
        }
      }),
      global::SETTER_MAP_FN.with(|setter| {
        ExternalReference {
          named_setter: *setter,
        }
      }),
      global::QUERY_MAP_FN.with(|query| {
        ExternalReference {
          named_query: *query,
        }
      }),
      global::DELETER_MAP_FN.with(|deleter| {
        ExternalReference {
          named_deleter: *deleter,
        }
      }),
      global::ENUMERATOR_MAP_FN.with(|enumerator| {
        ExternalReference {
          enumerator: *enumerator,
        }
      }),
      global::DEFINER_MAP_FN.with(|definer| {
        ExternalReference {
          named_definer: *definer,
        }
      }),
      global::DESCRIPTOR_MAP_FN.with(|descriptor| {
        ExternalReference {
          named_getter: *descriptor,
        }
      }),
    ];

    ext.external_references.to_mut().extend(external_references);
  },
);

#[sys_traits::auto_impl]
pub trait ExtNodeSys:
  sys_traits::BaseFsCanonicalize
  + sys_traits::BaseFsMetadata
  + sys_traits::BaseFsRead
  + sys_traits::EnvCurrentDir
  + Clone
{
}

pub type NodeResolver<TInNpmPackageChecker, TNpmPackageFolderResolver, TSys> =
  node_resolver::NodeResolver<
    TInNpmPackageChecker,
    DenoIsBuiltInNodeModuleChecker,
    TNpmPackageFolderResolver,
    TSys,
  >;
#[allow(clippy::disallowed_types)]
pub type NodeResolverRc<TInNpmPackageChecker, TNpmPackageFolderResolver, TSys> =
  deno_fs::sync::MaybeArc<
    NodeResolver<TInNpmPackageChecker, TNpmPackageFolderResolver, TSys>,
  >;

#[allow(clippy::disallowed_types)]
pub fn create_host_defined_options<'s>(
  scope: &mut v8::HandleScope<'s>,
) -> v8::Local<'s, v8::Data> {
  let host_defined_options = v8::PrimitiveArray::new(scope, 1);
  let value = v8::Boolean::new(scope, true);
  host_defined_options.set(scope, 0, value.into());
  host_defined_options.into()
}
