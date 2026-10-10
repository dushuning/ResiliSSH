mod browse;
mod client;
mod download;
mod errors;

pub use errors::is_remote_path_message;
mod host_key;
mod preflight;
mod remote_command;
mod transfer_common;
mod upload;

pub use browse::{create_remote_directory, list_remote_directory, RemoteDirListing};
pub use client::{
    abort_socket, connect_session, open_handshake, test_connection, AuthMethod, ConnectionConfig,
};
pub use host_key::{inspect_host_key, trust_host_key, HostKeyInfo};
pub use preflight::{bytes_needed, local_sha256, remote_free_bytes, remote_sha256};
pub use remote_command::{run_remote_command, CommandContext, CommandOutcome};
pub use download::{
    download_with_resume, remote_file_name, resolve_local_path, DownloadCallbacks, DownloadError,
};
pub use upload::{
    format_remote_sftp_error, remote_parent_status, resolve_remote_path, resolve_upload_target,
    upload_with_resume, UploadCallbacks, UploadError,
};
