mod browse;
mod client;
mod download;
mod errors;

pub use errors::is_remote_path_message;
mod host_key;
mod plan;
mod pool;
mod preflight;
mod remote_command;
mod remote_hash;
mod transfer_common;
mod tunnel;
mod upload;

pub use browse::{
    create_remote_directory, expand_remote_paths, list_remote_directory, remote_paths_exist,
    RemoteDirListing,
};
pub use client::{
    abort_socket, is_sftp_not_found, open_handshake, test_connection, AbortHandles, AuthMethod, ConnectError,
    ConnectionConfig,
    MAX_JUMP_HOPS,
};
#[cfg(test)]
pub(crate) use client::connect_session;
pub use host_key::{app_known_hosts_path, inspect_host_key, trust_host_key, HostKeyInfo};
pub use plan::{RangeSet, DEFAULT_CONNECTIONS};
#[cfg(test)]
pub(crate) use remote_hash::{remote_file_sha256, BlockHasher};
pub use pool::{acquire as acquire_session, release as release_session, with_session};
#[cfg(test)]
pub(crate) use pool::{clear as clear_session_pool, has_idle as session_pool_has_idle};
pub use preflight::{
    bytes_needed, local_free_bytes, local_sha256, remote_free_bytes, remote_sha256,
};
pub use remote_command::{run_remote_command, CommandContext, CommandOutcome};
pub use download::{
    download_with_resume, remote_file_name, resolve_local_path, DownloadCallbacks, DownloadError,
};
#[cfg(test)]
pub(crate) use upload::promote_staged_file;
pub use upload::{
    format_remote_sftp_error, remote_parent_status, resolve_remote_path, resolve_upload_target,
    is_staging_file, staging_path, upload_with_resume, UploadCallbacks, UploadError,
};
