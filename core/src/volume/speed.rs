//! Volume speed testing functionality

use crate::volume::{
	error::{VolumeError, VolumeResult},
	types::{MountType, Volume, VolumeType},
};
use std::time::Instant;
use tokio::{
	fs::{File, OpenOptions},
	io::{AsyncReadExt, AsyncWriteExt},
	time::{timeout, Duration},
};
use tracing::{debug, instrument, warn};

/// Configuration for speed tests
#[derive(Debug, Clone)]
pub struct SpeedTestConfig {
	/// Size of the test file in megabytes
	pub file_size_mb: usize,
	/// Timeout for the test in seconds
	pub timeout_secs: u64,
	/// Number of test iterations for averaging
	pub iterations: usize,
}

impl Default for SpeedTestConfig {
	fn default() -> Self {
		Self {
			file_size_mb: 10,
			timeout_secs: 30,
			iterations: 1,
		}
	}
}

/// Result of a speed test
#[derive(Debug, Clone)]
pub struct SpeedTestResult {
	/// Write speed in MB/s
	pub write_speed_mbps: f64,
	/// Read speed in MB/s
	pub read_speed_mbps: f64,
	/// Total time taken for the test
	pub duration_secs: f64,
}

/// Run a speed test on the given volume
#[instrument(skip(volume), fields(volume_name = %volume.name))]
pub async fn run_speed_test(volume: &Volume) -> VolumeResult<(u64, u64)> {
	run_speed_test_with_config(volume, SpeedTestConfig::default()).await
}

/// Run a speed test with custom configuration
#[instrument(skip(volume, config), fields(volume_name = %volume.name))]
pub async fn run_speed_test_with_config(
	volume: &Volume,
	config: SpeedTestConfig,
) -> VolumeResult<(u64, u64)> {
	if !volume.is_mounted {
		return Err(VolumeError::NotMounted(volume.name.clone()));
	}

	if volume.is_read_only {
		return Err(VolumeError::ReadOnly(volume.name.clone()));
	}

	debug!("Starting speed test with config: {:?}", config);

	let test_location = TestLocation::new(&volume.mount_point, &volume.mount_type).await?;
	let result = perform_speed_test(&test_location, &config).await;

	// A failed or timed-out test must not leave its directory on the user's volume.
	test_location.cleanup().await?;
	let result = result?;

	debug!(
		"Speed test completed: {:.2} MB/s write, {:.2} MB/s read",
		result.write_speed_mbps, result.read_speed_mbps
	);

	Ok((
		result.read_speed_mbps as u64,
		result.write_speed_mbps as u64,
	))
}

/// Helper for managing test files and directories
struct TestLocation {
	test_file: std::path::PathBuf,
	created_dir: Option<std::path::PathBuf>,
}

impl TestLocation {
	/// Create a new test location
	async fn new(volume_path: &std::path::Path, mount_type: &MountType) -> VolumeResult<Self> {
		let (dir, created_dir) = get_writable_directory(volume_path, mount_type).await?;
		// Unique so the root fallback can never overwrite a user's file.
		let test_file = dir.join(format!(
			".spacedrive_speed_test_{}.tmp",
			uuid::Uuid::new_v4()
		));

		Ok(Self {
			test_file,
			created_dir,
		})
	}

	/// Clean up test files and directories
	async fn cleanup(&self) -> VolumeResult<()> {
		// Remove test file
		if self.test_file.exists() {
			if let Err(e) = tokio::fs::remove_file(&self.test_file).await {
				warn!("Failed to remove test file: {}", e);
			}
		}

		// Non-recursive on purpose: if the directory holds anything besides our
		// test file, it is not ours to delete.
		if let Some(ref dir) = self.created_dir {
			if let Err(e) = tokio::fs::remove_dir(dir).await {
				warn!("Failed to remove test directory: {}", e);
			}
		}

		Ok(())
	}
}

/// Perform the actual speed test
async fn perform_speed_test(
	location: &TestLocation,
	config: &SpeedTestConfig,
) -> VolumeResult<SpeedTestResult> {
	let test_data = generate_test_data(config.file_size_mb);
	let timeout_duration = Duration::from_secs(config.timeout_secs);

	let mut write_speeds = Vec::new();
	let mut read_speeds = Vec::new();
	let overall_start = Instant::now();

	for iteration in 0..config.iterations {
		debug!(
			"Speed test iteration {}/{}",
			iteration + 1,
			config.iterations
		);

		// Write test
		let write_speed = timeout(
			timeout_duration,
			perform_write_test(&location.test_file, &test_data),
		)
		.await
		.map_err(|_| VolumeError::Timeout)??;

		write_speeds.push(write_speed);

		// Read test
		let read_speed = timeout(
			timeout_duration,
			perform_read_test(&location.test_file, test_data.len()),
		)
		.await
		.map_err(|_| VolumeError::Timeout)??;

		read_speeds.push(read_speed);

		// Clean up test file between iterations
		if iteration < config.iterations - 1 {
			let _ = tokio::fs::remove_file(&location.test_file).await;
		}
	}

	let avg_write_speed = write_speeds.iter().sum::<f64>() / write_speeds.len() as f64;
	let avg_read_speed = read_speeds.iter().sum::<f64>() / read_speeds.len() as f64;

	Ok(SpeedTestResult {
		write_speed_mbps: avg_write_speed,
		read_speed_mbps: avg_read_speed,
		duration_secs: overall_start.elapsed().as_secs_f64(),
	})
}

/// Generate test data for speed testing
fn generate_test_data(size_mb: usize) -> Vec<u8> {
	let size_bytes = size_mb * 1024 * 1024;

	// Use a pattern instead of zeros to avoid compression optimizations
	let pattern = b"SpacedriveSpeedTest0123456789ABCDEF";
	let mut data = Vec::with_capacity(size_bytes);

	for i in 0..size_bytes {
		data.push(pattern[i % pattern.len()]);
	}

	data
}

/// Perform write speed test
async fn perform_write_test(file_path: &std::path::Path, data: &[u8]) -> VolumeResult<f64> {
	let start = Instant::now();

	let mut file = OpenOptions::new()
		.write(true)
		.create(true)
		.truncate(true)
		.open(file_path)
		.await?;

	file.write_all(data).await?;
	file.sync_all().await?; // Ensure data is written to disk

	let duration = start.elapsed();
	let speed_mbps = (data.len() as f64 / 1024.0 / 1024.0) / duration.as_secs_f64();

	Ok(speed_mbps)
}

/// Perform read speed test
async fn perform_read_test(file_path: &std::path::Path, expected_size: usize) -> VolumeResult<f64> {
	let start = Instant::now();

	let mut file = File::open(file_path).await?;
	let mut buffer = Vec::with_capacity(expected_size);
	file.read_to_end(&mut buffer).await?;

	let duration = start.elapsed();
	let speed_mbps = (buffer.len() as f64 / 1024.0 / 1024.0) / duration.as_secs_f64();

	Ok(speed_mbps)
}

/// Get a writable directory within the volume.
///
/// Returns the directory to write in, plus the directory to remove afterwards
/// when this call created it. Ownership is proven by `create_dir` succeeding
/// on a fresh unique name, never inferred from a name, because the volume
/// root holds user data: an earlier version reused `<root>/tmp` and then
/// deleted it recursively, taking the user's own `tmp` folder with it (#3056).
async fn get_writable_directory(
	volume_path: &std::path::Path,
	mount_type: &MountType,
) -> VolumeResult<(std::path::PathBuf, Option<std::path::PathBuf>)> {
	if let MountType::System = mount_type {
		return Ok((std::env::temp_dir(), None));
	}

	let owned_dir = volume_path.join(format!(".spacedrive_speed_test_{}", uuid::Uuid::new_v4()));
	if tokio::fs::create_dir(&owned_dir).await.is_ok() {
		if can_write(&owned_dir).await {
			return Ok((owned_dir.clone(), Some(owned_dir)));
		}
		let _ = tokio::fs::remove_dir(&owned_dir).await;
	}

	// Some volumes allow file writes at the root but not new directories.
	if can_write(volume_path).await {
		return Ok((volume_path.to_path_buf(), None));
	}

	Err(VolumeError::PermissionDenied(format!(
		"No writable directory found in volume: {}",
		volume_path.display()
	)))
}

async fn can_write(dir: &std::path::Path) -> bool {
	let probe = dir.join(format!(".spacedrive_write_probe_{}", uuid::Uuid::new_v4()));
	let writable = tokio::fs::write(&probe, b"test").await.is_ok();
	let _ = tokio::fs::remove_file(&probe).await;
	writable
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::volume::{
		types::{DiskType, FileSystem},
		VolumeFingerprint,
	};
	use tempfile::TempDir;

	#[tokio::test]
	async fn test_speed_test_config() {
		let config = SpeedTestConfig::default();
		assert_eq!(config.file_size_mb, 10);
		assert_eq!(config.timeout_secs, 30);
		assert_eq!(config.iterations, 1);
	}

	#[tokio::test]
	async fn test_generate_test_data() {
		let data = generate_test_data(1); // 1MB
		assert_eq!(data.len(), 1024 * 1024);

		// Verify pattern is not all zeros
		assert!(data.iter().any(|&b| b != 0));
	}

	#[tokio::test]
	async fn test_writable_directory_external() {
		let temp_dir = TempDir::new().unwrap();
		let volume_path = temp_dir.path();

		let (writable_dir, created_dir) = get_writable_directory(volume_path, &MountType::External)
			.await
			.unwrap();

		assert!(writable_dir.exists());

		// Cleanup if we created a directory
		if let Some(dir) = created_dir {
			let _ = tokio::fs::remove_dir_all(dir).await;
		}
	}

	#[tokio::test]
	async fn test_writable_directory_system() {
		let (writable_dir, created_dir) =
			get_writable_directory(&std::path::PathBuf::from("/"), &MountType::System)
				.await
				.unwrap();

		assert!(writable_dir.exists());
		assert!(created_dir.is_none()); // Should use system temp, not create new dir
	}

	fn external_volume(mount_path: std::path::PathBuf) -> Volume {
		let device_id = uuid::Uuid::new_v4();
		let fingerprint = VolumeFingerprint::from_primary_volume(&mount_path, device_id);
		let now = chrono::Utc::now();

		Volume {
			id: uuid::Uuid::new_v4(),
			fingerprint,
			cloud_identifier: None,
			cloud_config: None,
			device_id,
			name: "Test Volume".to_string(),
			library_id: None,
			is_tracked: false,
			mount_point: mount_path.clone(),
			mount_points: vec![mount_path],
			volume_type: VolumeType::External,
			mount_type: MountType::External,
			disk_type: DiskType::Unknown,
			file_system: FileSystem::Other("test".to_string()),
			total_capacity: 1000000000,
			available_space: 500000000,
			is_read_only: false,
			is_mounted: true,
			locked: false,
			hardware_id: None,
			backend: None,
			apfs_container: None,
			container_volume_id: None,
			path_mappings: Vec::new(),
			is_user_visible: true,
			auto_track_eligible: false,
			read_speed_mbps: None,
			write_speed_mbps: None,
			created_at: now,
			updated_at: now,
			last_seen_at: now,
			total_files: None,
			total_directories: None,
			unique_bytes: None,
			indexed_bytes: None,
			last_stats_update: None,
			display_name: Some("Test Volume".to_string()),
			is_favorite: false,
			color: None,
			icon: None,
			error_message: None,
			supports_block_cloning: false,
		}
	}

	fn small_config() -> SpeedTestConfig {
		SpeedTestConfig {
			file_size_mb: 1,
			timeout_secs: 10,
			iterations: 1,
		}
	}

	fn dir_names(path: &std::path::Path) -> Vec<String> {
		let mut names: Vec<String> = std::fs::read_dir(path)
			.unwrap()
			.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
			.collect();
		names.sort();
		names
	}

	#[tokio::test]
	async fn test_full_speed_test() {
		let temp_dir = TempDir::new().unwrap();
		let volume = external_volume(temp_dir.path().to_path_buf());

		let (read_speed, write_speed) = run_speed_test_with_config(&volume, small_config())
			.await
			.unwrap();
		assert!(read_speed > 0);
		assert!(write_speed > 0);

		assert!(
			dir_names(temp_dir.path()).is_empty(),
			"speed test left files behind"
		);
	}

	// Regression for #3056: a user's root-level `tmp` folder was deleted recursively.
	#[tokio::test]
	async fn speed_test_preserves_existing_root_folders() {
		let temp_dir = TempDir::new().unwrap();
		let root = temp_dir.path();
		for name in ["tmp", ".spacedrive_temp"] {
			std::fs::create_dir(root.join(name)).unwrap();
			std::fs::write(root.join(name).join("keep.txt"), b"user data").unwrap();
		}
		std::fs::write(root.join("spacedrive_speed_test.tmp"), b"user file").unwrap();

		run_speed_test_with_config(&external_volume(root.to_path_buf()), small_config())
			.await
			.unwrap();

		assert_eq!(
			dir_names(root),
			vec![".spacedrive_temp", "spacedrive_speed_test.tmp", "tmp"]
		);
		for name in ["tmp", ".spacedrive_temp"] {
			assert_eq!(
				std::fs::read(root.join(name).join("keep.txt")).unwrap(),
				b"user data"
			);
		}
		assert_eq!(
			std::fs::read(root.join("spacedrive_speed_test.tmp")).unwrap(),
			b"user file"
		);
	}

	#[tokio::test]
	async fn cleanup_never_removes_a_directory_with_other_contents() {
		let temp_dir = TempDir::new().unwrap();
		let location = TestLocation::new(temp_dir.path(), &MountType::External)
			.await
			.unwrap();
		let dir = location
			.created_dir
			.clone()
			.expect("test owns a fresh directory");
		std::fs::write(dir.join("unexpected.txt"), b"not ours").unwrap();

		location.cleanup().await.unwrap();

		assert!(dir.join("unexpected.txt").exists());
	}
}
