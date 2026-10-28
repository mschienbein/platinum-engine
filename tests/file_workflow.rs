use platinum_engine::file_workflow::{
    self, AudioFormat, Cancellation, FileIdentity, OutputMode, OutputPlan, OutputSettings, ProcessingState,
    RestoreStatus, SourceCodec, SourceType, StagedOutput, WorkflowError,
};
use platinum_engine::metadata::{Marker, MarkerText, ProcessingMarker};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        for _ in 0..128 {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("platinum-file-workflow-{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(fs::canonicalize(path).unwrap()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create isolated fixture {}: {e}", path.display()),
            }
        }
        panic!("could not allocate isolated fixture")
    }
    fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name); fs::create_dir_all(&path).unwrap(); path
    }
    fn file(&self, name: &str, contents: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        file.write_all(contents).unwrap(); path
    }
}
impl Drop for Fixture { fn drop(&mut self) { fs::remove_dir_all(&self.0).unwrap(); } }

fn selected(path: &Path) -> file_workflow::DiscoveredFile {
    let mut files = file_workflow::discover_inputs(&[path.to_owned()]).unwrap();
    assert_eq!(files.len(), 1); files.remove(0)
}
fn same_folder(format: AudioFormat) -> OutputSettings { OutputSettings { mode: OutputMode::SameFolder, format } }
fn replacement(dir: &Path, format: AudioFormat) -> OutputSettings {
    OutputSettings { mode: OutputMode::Replace { backup_directory: dir.to_owned() }, format }
}
fn begin(plan: &OutputPlan) -> Result<StagedOutput, WorkflowError> {
    plan.begin(&FileIdentity::capture(plan.source())?)
}
fn workspace_directory(tx: &StagedOutput, parent: &Path) -> PathBuf {
    let alias = fs::metadata(tx.staging_path().parent().unwrap()).unwrap();
    let mut matching = fs::read_dir(parent).unwrap().map(|entry| entry.unwrap().path()).filter(|path| {
        fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()
            && (metadata.dev(), metadata.ino()) == (alias.dev(), alias.ino()))
    });
    let directory = matching.next().expect("workspace inode must have a directory entry");
    assert!(matching.next().is_none(), "workspace inode has multiple directory entries");
    directory
}
fn authorize_overwrite(plans: &mut [OutputPlan], inputs: &[file_workflow::DiscoveredFile]) {
    let destinations: Vec<_> = plans.iter().map(|plan| plan.destination().to_owned()).collect();
    let guard = Arc::new(file_workflow::OverwriteGuard::new(inputs, &destinations).unwrap());
    for plan in plans { plan.enable_overwrite(Arc::clone(&guard)).unwrap(); }
}
fn marker(backup: Option<&Path>) -> Marker {
    Marker::from_processing(ProcessingMarker {
        software_version: None, backup_file_name: backup.map(|p| p.to_str().unwrap()),
        applied_clip_repair: false, applied_pitch_correction: false, applied_warmth: false,
        expansion_applied: 0, compression_applied: 0, volume_analysis_version: 4,
        target_volume: -12.0, input_volume: -14.0, output_volume: -12.0,
        clip_repair_count: 0, exciter_option: 0, input_pitch: 0.0, output_pitch: 0.0,
        speed_adjustment: 1.0,
    })
}
fn stage(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path).unwrap();
    file.write_all(bytes).unwrap(); file.sync_all().unwrap();
}

#[test]
fn discovery_is_recursive_sorted_deduplicated_and_explicit_about_protected_and_ogg() {
    let f = Fixture::new();
    let root = f.dir("Album");
    let wav = f.file("Album/disc/z.WAV", b"wav");
    let mp3 = f.file("Album/a.mp3", b"mp3");
    let protected = f.file("Album/protected.M4P", b"protected");
    let wl = f.file("Album/disc/x.WL.MP3", b"protected");
    let ogg = f.file("Album/advertised.ogg", b"ogg");
    f.file("Album/ignore.txt", b"not audio");
    symlink(&mp3, root.join("link.mp3")).unwrap();
    symlink(root.join("disc"), root.join("linked-directory")).unwrap();
    let files = file_workflow::discover_inputs(&[root.clone(), wav.clone()]).unwrap();
    let mut expected = vec![wav.clone(), mp3.clone(), protected.clone(), wl.clone(), ogg.clone()];
    expected.sort();
    assert_eq!(files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(), expected);
    let wav_entry = files.iter().find(|v| v.path == wav).unwrap();
    assert_eq!(wav_entry.relative_path, PathBuf::from("Album/disc/z.WAV"));
    assert_eq!(files.iter().find(|v| v.path == protected).unwrap().source_type, SourceType::ProtectedM4p);
    assert_eq!(files.iter().find(|v| v.path == wl).unwrap().source_type, SourceType::ProtectedWlMp3);
    let ogg_entry = files.iter().find(|v| v.path == ogg).unwrap();
    assert_eq!(ogg_entry.source_type, SourceType::OggUnverified);
    assert!(matches!(OutputPlan::new(ogg_entry, SourceCodec::Other(0), &same_folder(AudioFormat::Wav), None), Err(WorkflowError::UnsupportedSource(_))));
    let protected_entry = files.iter().find(|v| v.path == protected).unwrap();
    assert!(matches!(OutputPlan::new(protected_entry, SourceCodec::Aac, &same_folder(AudioFormat::Wav), None), Err(WorkflowError::ProtectedSource(_))));
    assert!(matches!(file_workflow::discover_inputs(&[root.join("link.mp3")]), Err(WorkflowError::InvalidPath { .. })));
}

#[test]
fn advertised_ogg_extensions_follow_discovery_boundaries() {
    // The reference admission table has neither ogg nor oga. Our discovery
    // reports .ogg explicitly, including uppercase, and ignores .oga.
    let f = Fixture::new();
    let uppercase = f.file("upper.OGG", b"synthetic input");
    assert_eq!(selected(&uppercase).source_type, SourceType::OggUnverified);
    let oga = f.file("unadvertised.oga", b"synthetic input");
    assert!(file_workflow::discover_inputs(&[oga]).unwrap().is_empty());
}

#[test]
fn processed_marker_wins_and_suffix_is_exact_and_case_sensitive() {
    let marked = marker(Some(Path::new("/synthetic/backup.wav")));
    assert_eq!(file_workflow::processed_state(Path::new("song_PN.wav"), Some(&marked)).unwrap(),
        ProcessingState::Marked { backup: Some(PathBuf::from("/synthetic/backup.wav")) });
    let mut unprocessed_marker = marked.clone(); unprocessed_marker.is_processed = false;
    assert_eq!(file_workflow::processed_state(Path::new("song_PN.wav"), Some(&unprocessed_marker)).unwrap(), ProcessingState::Suffixed);
    assert_eq!(file_workflow::processed_state(Path::new("song_pn.wav"), None).unwrap(), ProcessingState::Unprocessed);
    assert_eq!(file_workflow::processed_state(Path::new("song_PNx.wav"), None).unwrap(), ProcessingState::Unprocessed);
    let mut bad = marked; bad.backup_file_name = Some(MarkerText::Utf16(vec![0xd800]));
    assert!(matches!(file_workflow::processed_state(Path::new("song.wav"), Some(&bad)), Err(WorkflowError::InvalidMarkerBackup)));
    let f = Fixture::new(); let path = f.file("already_PN.wav", b"source");
    assert!(matches!(OutputPlan::new(&selected(&path), SourceCodec::Lpcm, &same_folder(AudioFormat::Wav), None), Err(WorkflowError::AlreadyProcessed { state: ProcessingState::Suffixed, .. })));
}

#[test]
fn formats_use_detected_codec_and_replacement_forces_match_input() {
    let f = Fixture::new(); let backup = f.dir("backup");
    let m4a = f.file("input.m4a", b"synthetic"); let item = selected(&m4a);
    let aac = OutputPlan::new(&item, SourceCodec::Aac, &same_folder(AudioFormat::MatchInput), None).unwrap();
    let alac = OutputPlan::new(&item, SourceCodec::Alac, &same_folder(AudioFormat::MatchInput), None).unwrap();
    assert_eq!(aac.format(), AudioFormat::Aac); assert_eq!(alac.format(), AudioFormat::Alac);
    assert_eq!(aac.destination(), f.0.join("input_PN.m4a"));
    let forced = OutputPlan::new(&item, SourceCodec::Aac, &replacement(&backup, AudioFormat::Flac), None).unwrap();
    assert_eq!(forced.format(), AudioFormat::Aac); assert_eq!(forced.requested_format(), AudioFormat::MatchInput);
    assert_eq!(forced.destination(), m4a);
    assert!(matches!(OutputPlan::new(&item, SourceCodec::Other(0x12345678), &same_folder(AudioFormat::MatchInput), None), Err(WorkflowError::UnsupportedCodec(0x12345678))));
    let aif = f.file("tune.AIF", b"synthetic");
    let item = selected(&aif);
    let matching = OutputPlan::new(&item, SourceCodec::Lpcm, &same_folder(AudioFormat::MatchInput), None).unwrap();
    assert_eq!(matching.format(), AudioFormat::Aiff); assert_eq!(matching.destination(), f.0.join("tune_PN.aif"));
    let flac = f.file("input.flac", b"synthetic");
    assert_eq!(OutputPlan::new(&selected(&flac), SourceCodec::Lpcm, &same_folder(AudioFormat::MatchInput), None).unwrap().format(), AudioFormat::Flac);
    let wav = f.file("misnamed.wav", b"synthetic");
    let decoded_aac = OutputPlan::new(&selected(&wav), SourceCodec::Aac, &replacement(&backup, AudioFormat::Mp3), None).unwrap();
    assert_eq!(decoded_aac.destination(), f.0.join("misnamed.m4a"));
}

#[test]
fn replacement_preserves_matching_extension_spelling() {
    let f = Fixture::new(); let backups = f.dir("backups");
    for (name, codec) in [("one.WAV", SourceCodec::Lpcm), ("two.AIF", SourceCodec::Lpcm),
        ("three.AIFF", SourceCodec::Lpcm), ("four.M4A", SourceCodec::Aac)] {
        let source = f.file(name, b"original");
        let plan = OutputPlan::new(&selected(&source), codec, &replacement(&backups, AudioFormat::Wav), None).unwrap();
        assert_eq!(plan.destination().as_os_str(), source.as_os_str());
        let tx = begin(&plan).unwrap();
        stage(tx.staging_path(), b"encoded");
        let result = tx.finish(&Cancellation::new()).unwrap();
        assert_eq!(result.destination.as_os_str(), source.as_os_str());
        assert_eq!(fs::read(&source).unwrap(), b"encoded");
        assert_eq!(fs::read(result.backup.unwrap()).unwrap(), b"original");
        let names: Vec<_> = fs::read_dir(&f.0).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        assert!(names.contains(&source.file_name().unwrap().to_os_string()));
    }
}

#[test]
fn mirrored_paths_follow_selection_root_and_batch_collisions_are_rejected() {
    let f = Fixture::new(); let album = f.dir("input/Album"); let out = f.dir("output");
    f.file("input/Album/sub/song.wav", b"one"); f.file("input/Album/other/song.wav", b"two");
    let files = file_workflow::discover_inputs(&[album]).unwrap();
    let flat = OutputSettings { mode: OutputMode::SpecificFolder { directory: out.clone(), mirror: false }, format: AudioFormat::Wav };
    let plans: Vec<_> = files.iter().map(|x| OutputPlan::new(x, SourceCodec::Lpcm, &flat, None).unwrap()).collect();
    assert!(matches!(file_workflow::validate_batch(&plans, &files), Err(WorkflowError::DestinationConflict { .. })));
    let mirror = OutputSettings { mode: OutputMode::SpecificFolder { directory: out.clone(), mirror: true }, format: AudioFormat::Wav };
    let plans: Vec<_> = files.iter().map(|x| OutputPlan::new(x, SourceCodec::Lpcm, &mirror, None).unwrap()).collect();
    file_workflow::validate_batch(&plans, &files).unwrap();
    let borrowed: Vec<_> = plans.iter().collect();
    file_workflow::validate_batch(&borrowed, &files).unwrap();
    assert!(plans.iter().any(|p| p.destination() == out.join("Album/sub/song_PN.wav")));
    assert!(plans.iter().any(|p| p.destination() == out.join("Album/other/song_PN.wav")));
    let tx = begin(&plans[0]).unwrap(); stage(tx.staging_path(), b"encoded");
    let published = tx.finish(&Cancellation::new()).unwrap(); assert_eq!(fs::read(published.destination).unwrap(), b"encoded");
}

#[test]
fn ordinary_output_reserves_and_publishes_without_clobbering() {
    let f = Fixture::new(); let source = f.file("song.wav", b"original");
    let plan = OutputPlan::new(&selected(&source), SourceCodec::Lpcm, &same_folder(AudioFormat::Wav), None).unwrap();
    let tx = begin(&plan).unwrap();
    assert!(matches!(begin(&plan).err().unwrap(), WorkflowError::ReservationExists(_)));
    let workspace = workspace_directory(&tx, plan.destination().parent().unwrap());
    stage(tx.staging_path(), b"complete output");
    let published = tx.finish(&Cancellation::new()).unwrap();
    assert_eq!(fs::read(&published.destination).unwrap(), b"complete output");
    assert_eq!(fs::read(&source).unwrap(), b"original"); assert!(published.backup.is_none());
    assert!(!published.replaced_existing_output);
    assert!(!workspace.exists());
    assert!(matches!(begin(&plan).err().unwrap(), WorkflowError::DestinationExists(_)));
    assert_eq!(fs::read(&published.destination).unwrap(), b"complete output");
}

#[test]
fn late_output_collision_fails_without_overwriting_either_file() {
    let f = Fixture::new(); let source = f.file("track.wav", b"original");
    let plan = OutputPlan::new(&selected(&source), SourceCodec::Lpcm, &same_folder(AudioFormat::Wav), None).unwrap();
    let tx = begin(&plan).unwrap(); stage(tx.staging_path(), b"candidate");
    stage(plan.destination(), b"other writer");
    let error = tx.finish(&Cancellation::new()).unwrap_err();
    assert!(matches!(error, WorkflowError::Transaction { cause, published: false, .. } if matches!(*cause, WorkflowError::DestinationExists(_))));
    assert_eq!(fs::read(&source).unwrap(), b"original");
    assert_eq!(fs::read(plan.destination()).unwrap(), b"other writer");
}

#[test]
fn overwrite_atomically_replaces_existing_output_and_preserves_source() {
    let f = Fixture::new(); let source = f.file("song.wav", b"original");
    let destination = f.file("song_PN.wav", b"old output");
    let source_metadata = fs::metadata(&source).unwrap();
    let old_output = fs::metadata(&destination).unwrap();
    let files = vec![selected(&source)];
    let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &same_folder(AudioFormat::Wav), None).unwrap();
    authorize_overwrite(std::slice::from_mut(&mut plan), &files);
    file_workflow::validate_batch(&[&plan], &files).unwrap();
    let tx = begin(&plan).unwrap(); stage(tx.staging_path(), b"new complete output");
    assert_eq!(fs::read(&destination).unwrap(), b"old output");
    let result = tx.finish(&Cancellation::new()).unwrap();
    assert!(result.replaced_existing_output); assert!(result.backup.is_none());
    assert_eq!(fs::read(&destination).unwrap(), b"new complete output");
    assert_ne!(fs::metadata(&destination).unwrap().ino(), old_output.ino());
    assert_eq!(fs::read(&source).unwrap(), b"original");
    assert_eq!(fs::metadata(&source).unwrap().ino(), source_metadata.ino());
}

#[test]
fn overwrite_of_missing_destination_reports_no_replacement() {
    let f = Fixture::new(); let source = f.file("song.wav", b"original");
    let files = vec![selected(&source)];
    let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &same_folder(AudioFormat::Wav), None).unwrap();
    authorize_overwrite(std::slice::from_mut(&mut plan), &files);
    let tx = begin(&plan).unwrap(); stage(tx.staging_path(), b"new output");
    assert!(!tx.finish(&Cancellation::new()).unwrap().replaced_existing_output);
}

#[test]
fn overwrite_refuses_selected_sources_hard_links_symlinks_and_directories() {
    let f = Fixture::new(); let source = f.file("song.wav", b"source");
    let destination = f.0.join("song_PN.wav");
    let files = vec![selected(&source)];
    let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &same_folder(AudioFormat::Wav), None).unwrap();
    fs::hard_link(&source, &destination).unwrap();
    authorize_overwrite(std::slice::from_mut(&mut plan), &files);
    assert!(matches!(file_workflow::validate_batch(&[&plan], &files), Err(WorkflowError::DestinationConflict { .. })));
    assert_eq!(fs::read(&source).unwrap(), b"source");
    fs::remove_file(&destination).unwrap();
    symlink(&source, &destination).unwrap();
    let files = vec![selected(&source)];
    authorize_overwrite(std::slice::from_mut(&mut plan), &files);
    assert!(matches!(file_workflow::validate_batch(&[&plan], &files), Err(WorkflowError::DestinationExists(_))));
    fs::remove_file(&destination).unwrap(); fs::create_dir(&destination).unwrap();
    assert!(matches!(file_workflow::validate_batch(&[&plan], &files), Err(WorkflowError::DestinationExists(_))));
    fs::remove_dir(&destination).unwrap();

    let selected_output = f.file("song_PN.wav", b"another selected input");
    let files = vec![selected(&source), selected(&selected_output)];
    authorize_overwrite(std::slice::from_mut(&mut plan), &files);
    assert!(matches!(file_workflow::validate_batch(&[&plan], &files), Err(WorkflowError::DestinationConflict { .. })));
    fs::remove_file(&selected_output).unwrap();
    let other_source = f.file("other.wav", b"another input");
    fs::hard_link(&other_source, &destination).unwrap();
    let files = vec![selected(&source), selected(&other_source)];
    authorize_overwrite(std::slice::from_mut(&mut plan), &files);
    assert!(matches!(file_workflow::validate_batch(&[&plan], &files), Err(WorkflowError::DestinationConflict { .. })));
    assert_eq!(fs::read(&other_source).unwrap(), b"another input");
    assert_eq!(fs::read(&source).unwrap(), b"source");
}

#[test]
fn overwrite_refuses_duplicate_and_hard_linked_batch_destinations() {
    let f = Fixture::new();
    let first = f.file("a/song.wav", b"first"); let second = f.file("b/song.wav", b"second");
    let output = f.dir("output");
    let settings = OutputSettings { mode: OutputMode::SpecificFolder { directory: output, mirror: false }, format: AudioFormat::Wav };
    let files = vec![selected(&first), selected(&second)];
    let mut plans: Vec<_> = files.iter().map(|file| OutputPlan::new(file, SourceCodec::Lpcm, &settings, None).unwrap()).collect();
    authorize_overwrite(&mut plans, &files);
    assert!(matches!(file_workflow::validate_batch(&plans, &files), Err(WorkflowError::DestinationConflict { .. })));

    let first = f.file("one.wav", b"one"); let second = f.file("two.wav", b"two");
    let first_output = f.file("one_PN.wav", b"existing");
    fs::hard_link(&first_output, f.0.join("two_PN.wav")).unwrap();
    let files = vec![selected(&first), selected(&second)];
    let mut plans: Vec<_> = files.iter().map(|file| OutputPlan::new(file, SourceCodec::Lpcm, &same_folder(AudioFormat::Wav), None).unwrap()).collect();
    authorize_overwrite(&mut plans, &files);
    assert!(matches!(file_workflow::validate_batch(&plans, &files), Err(WorkflowError::DestinationConflict { .. })));
    assert_eq!(fs::read(first_output).unwrap(), b"existing");
}

#[test]
fn overwrite_preserves_destination_when_cancelled_or_changed_before_commit() {
    let f = Fixture::new(); let source = f.file("song.wav", b"source");
    let destination = f.file("song_PN.wav", b"old output");
    let files = vec![selected(&source)];
    let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &same_folder(AudioFormat::Wav), None).unwrap();
    authorize_overwrite(std::slice::from_mut(&mut plan), &files);
    let tx = begin(&plan).unwrap(); stage(tx.staging_path(), b"candidate");
    let cancelled = Cancellation::new(); cancelled.cancel();
    assert!(tx.finish(&cancelled).unwrap_err().is_cancelled());
    assert_eq!(fs::read(&destination).unwrap(), b"old output");
    let tx = begin(&plan).unwrap(); stage(tx.staging_path(), b"candidate");
    fs::write(&destination, b"changed by another writer").unwrap();
    assert!(matches!(tx.finish(&Cancellation::new()), Err(WorkflowError::Transaction { published: false, .. })));
    assert_eq!(fs::read(&destination).unwrap(), b"changed by another writer");
    assert_eq!(fs::read(&source).unwrap(), b"source");
}

#[test]
fn overwrite_does_not_clobber_a_destination_created_after_staging() {
    let f = Fixture::new(); let source = f.file("song.wav", b"source");
    let files = vec![selected(&source)];
    let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &same_folder(AudioFormat::Wav), None).unwrap();
    authorize_overwrite(std::slice::from_mut(&mut plan), &files);
    let tx = begin(&plan).unwrap(); stage(tx.staging_path(), b"candidate");
    stage(plan.destination(), b"late output");
    assert!(matches!(tx.finish(&Cancellation::new()), Err(WorkflowError::Transaction { published: false, .. })));
    assert_eq!(fs::read(plan.destination()).unwrap(), b"late output");
}

#[test]
fn overwrite_rejects_destinations_changed_between_preflight_and_begin() {
    let f = Fixture::new(); let source = f.file("song.wav", b"source");
    let files = vec![selected(&source)];
    let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &same_folder(AudioFormat::Wav), None).unwrap();
    authorize_overwrite(std::slice::from_mut(&mut plan), &files);
    file_workflow::validate_batch(&[&plan], &files).unwrap();
    stage(plan.destination(), b"created after preflight");
    assert!(matches!(begin(&plan), Err(WorkflowError::Transaction { cause, published: false, .. })
        if matches!(*cause, WorkflowError::DestinationChanged(_))));
    assert_eq!(fs::read(plan.destination()).unwrap(), b"created after preflight");

    authorize_overwrite(std::slice::from_mut(&mut plan), &files);
    file_workflow::validate_batch(&[&plan], &files).unwrap();
    let replacement = f.file("replacement.tmp", b"different file");
    fs::rename(replacement, plan.destination()).unwrap();
    assert!(matches!(begin(&plan), Err(WorkflowError::Transaction { cause, published: false, .. })
        if matches!(*cause, WorkflowError::DestinationChanged(_))));
    assert_eq!(fs::read(plan.destination()).unwrap(), b"different file");
    assert_eq!(fs::read(&source).unwrap(), b"source");
}

#[test]
fn overwrite_reports_permission_denied_for_unreadable_destination() {
    let f = Fixture::new(); let source = f.file("song.wav", b"source");
    let destination = f.file("song_PN.wav", b"old output");
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o200)).unwrap();
    let files = vec![selected(&source)];
    let mut plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &same_folder(AudioFormat::Wav), None).unwrap();
    authorize_overwrite(std::slice::from_mut(&mut plan), &files);
    file_workflow::validate_batch(&[&plan], &files).unwrap();
    let tx = begin(&plan).unwrap(); stage(tx.staging_path(), b"candidate");
    let error = tx.finish(&Cancellation::new()).unwrap_err();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(error, WorkflowError::Transaction { cause, published: false, .. }
        if matches!(&*cause, WorkflowError::Io { source, .. } if source.kind() == std::io::ErrorKind::PermissionDenied)));
    assert_eq!(fs::read(&destination).unwrap(), b"old output");
}

#[test]
fn batch_detects_case_equivalent_destinations_on_case_insensitive_volume() {
    let f = Fixture::new();
    let upper = f.file("a/Song.wav", b"upper"); let lower = f.file("b/song.wav", b"lower");
    let output = f.dir("output");
    let settings = OutputSettings { mode: OutputMode::SpecificFolder { directory: output.clone(), mirror: false }, format: AudioFormat::Wav };
    let files = vec![selected(&upper), selected(&lower)];
    let mut plans: Vec<_> = files.iter().map(|file| OutputPlan::new(file, SourceCodec::Lpcm, &settings, None).unwrap()).collect();
    authorize_overwrite(&mut plans, &files);
    let probe = output.join("CaseProbe"); fs::write(&probe, b"probe").unwrap();
    let insensitive = output.join("caseprobe").exists();
    fs::remove_file(probe).unwrap();
    if insensitive {
        assert!(matches!(file_workflow::validate_batch(&plans, &files), Err(WorkflowError::DestinationConflict { .. })));
    } else {
        file_workflow::validate_batch(&plans, &files).unwrap();
    }
    assert!(!plans[0].destination().exists()); assert!(!plans[1].destination().exists());
}

#[cfg(target_os = "macos")]
#[test]
fn batch_rejects_canonically_equivalent_destinations_before_publication() {
    let f = Fixture::new();
    let nfc = f.file("a/café.wav", b"first");
    let nfd = f.file("b/cafe\u{301}.wav", b"second");
    let distinct = f.file("c/caff.wav", b"third");
    let output = f.dir("output");
    let settings = OutputSettings { mode: OutputMode::SpecificFolder { directory: output.clone(), mirror: false }, format: AudioFormat::Wav };
    let files = vec![selected(&nfc), selected(&nfd), selected(&distinct)];
    let plans: Vec<_> = files.iter().map(|file| OutputPlan::new(file, SourceCodec::Lpcm, &settings, None).unwrap()).collect();
    assert!(matches!(file_workflow::validate_batch(&plans[..2], &files), Err(WorkflowError::DestinationConflict { .. })));
    file_workflow::validate_batch(&[&plans[0], &plans[2]], &files).unwrap();
    assert_eq!(fs::read_dir(&output).unwrap().count(), 0);
    assert_eq!(fs::read(nfc).unwrap(), b"first"); assert_eq!(fs::read(nfd).unwrap(), b"second");
}

#[test]
fn batch_case_and_normalization_follow_destination_volume_policy() {
    let f = Fixture::new();
    let upper = f.file("a/Café.wav", b"first");
    let lower = f.file("b/cafe\u{301}.wav", b"second");
    let output = f.dir("output");
    let settings = OutputSettings { mode: OutputMode::SpecificFolder { directory: output.clone(), mirror: false }, format: AudioFormat::Wav };
    let files = vec![selected(&upper), selected(&lower)];
    let plans: Vec<_> = files.iter().map(|file| OutputPlan::new(file, SourceCodec::Lpcm, &settings, None).unwrap()).collect();
    let probe = output.join("CaseProbe"); fs::write(&probe, b"probe").unwrap();
    let insensitive = output.join("caseprobe").exists(); fs::remove_file(probe).unwrap();
    if insensitive {
        assert!(matches!(file_workflow::validate_batch(&plans, &files), Err(WorkflowError::DestinationConflict { .. })));
    } else {
        file_workflow::validate_batch(&plans, &files).unwrap();
    }
    assert_eq!(fs::read_dir(&output).unwrap().count(), 0);
}

#[cfg(target_os = "macos")]
#[test]
fn batch_rejects_normalized_destination_matching_an_unplanned_selected_source() {
    let f = Fixture::new();
    let input = f.file("input/café.wav", b"original");
    let selected_output = f.file("output/cafe\u{301}_PN.wav", b"selected input");
    let output = f.0.join("output");
    let settings = OutputSettings { mode: OutputMode::SpecificFolder { directory: output.clone(), mirror: false }, format: AudioFormat::Wav };
    let files = vec![selected(&input), selected(&selected_output)];
    let plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &settings, None).unwrap();
    assert!(matches!(file_workflow::validate_batch(&[&plan], &files), Err(WorkflowError::DestinationConflict { .. })));
    assert_eq!(fs::read(&selected_output).unwrap(), b"selected input");
    assert_eq!(fs::read(&input).unwrap(), b"original");
    assert_eq!(fs::read_dir(output).unwrap().count(), 1);
}

#[test]
fn invalid_utf8_selected_name_does_not_block_unrelated_plan() {
    use std::os::unix::ffi::OsStringExt;
    let f = Fixture::new(); let input = f.file("song.wav", b"original");
    let invalid_name = std::ffi::OsString::from_vec(b"other\xff.wav".to_vec());
    let invalid = file_workflow::DiscoveredFile {
        path: f.0.join(&invalid_name), relative_path: PathBuf::from(invalid_name), source_type: SourceType::Wav,
    };
    let files = vec![selected(&input), invalid];
    let plan = OutputPlan::new(&files[0], SourceCodec::Lpcm, &same_folder(AudioFormat::Wav), None).unwrap();
    file_workflow::validate_batch(&[&plan], &files).unwrap();
    assert!(!plan.destination().exists()); assert_eq!(fs::read(input).unwrap(), b"original");
}

#[test]
fn replacement_reserves_verified_backup_with_reference_even_suffixes() {
    let f = Fixture::new(); let source = f.file("song.wav", b"original audio"); let backups = f.dir("backups");
    f.file("backups/song.wav", b"existing first"); f.file("backups/song_2.wav", b"existing second");
    let plan = OutputPlan::new(&selected(&source), SourceCodec::Lpcm, &replacement(&backups, AudioFormat::Mp3), None).unwrap();
    let tx = begin(&plan).unwrap();
    let chosen = tx.backup_path().unwrap().to_owned();
    assert_eq!(chosen, backups.join("song_4.wav"));
    assert_eq!(fs::read(&chosen).unwrap(), b"original audio");
    stage(tx.staging_path(), b"processed audio");
    let result = tx.finish(&Cancellation::new()).unwrap();
    assert_eq!(result.destination, source); assert_eq!(result.backup, Some(chosen.clone()));
    assert_eq!(fs::read(&source).unwrap(), b"processed audio"); assert_eq!(fs::read(&chosen).unwrap(), b"original audio");
    assert_eq!(fs::read(backups.join("song.wav")).unwrap(), b"existing first");
    assert_eq!(fs::read(backups.join("song_2.wav")).unwrap(), b"existing second");
}

#[test]
fn changed_backup_or_source_blocks_replacement_before_original_is_moved() {
    let f = Fixture::new(); let backups = f.dir("backups");
    let source = f.file("song.wav", b"original audio");
    let plan = OutputPlan::new(&selected(&source), SourceCodec::Lpcm, &replacement(&backups, AudioFormat::Wav), None).unwrap();
    let tx = begin(&plan).unwrap(); let backup = tx.backup_path().unwrap().to_owned();
    stage(tx.staging_path(), b"candidate"); fs::write(&backup, b"tampered backup").unwrap();
    let error = tx.finish(&Cancellation::new()).unwrap_err();
    assert!(matches!(error, WorkflowError::Transaction { cause, published: false, .. } if matches!(*cause, WorkflowError::BackupChanged(_))));
    assert_eq!(fs::read(&source).unwrap(), b"original audio"); assert_eq!(fs::read(&backup).unwrap(), b"tampered backup");
    let other = f.file("other.wav", b"before");
    let plan = OutputPlan::new(&selected(&other), SourceCodec::Lpcm, &replacement(&backups, AudioFormat::Wav), None).unwrap();
    let tx = begin(&plan).unwrap(); stage(tx.staging_path(), b"candidate"); fs::write(&other, b"changed by someone else").unwrap();
    assert!(matches!(tx.finish(&Cancellation::new()).unwrap_err(), WorkflowError::Transaction { cause, published: false, .. } if matches!(*cause, WorkflowError::SourceChanged(_))));
    assert_eq!(fs::read(&other).unwrap(), b"changed by someone else");
}

#[test]
fn abort_cleans_its_stage_and_reports_retained_backup() {
    let f = Fixture::new(); let source = f.file("song.wav", b"original"); let backups = f.dir("backups");
    let plan = OutputPlan::new(&selected(&source), SourceCodec::Lpcm, &replacement(&backups, AudioFormat::Wav), None).unwrap();
    let tx = begin(&plan).unwrap(); let workspace = workspace_directory(&tx, plan.destination().parent().unwrap());
    stage(tx.staging_path(), b"partial");
    tx.remember_staging_file().unwrap();
    let report = tx.abort().unwrap();
    assert_eq!(fs::read(report.retained_backup.unwrap()).unwrap(), b"original");
    assert_eq!(fs::read(source).unwrap(), b"original"); assert!(!workspace.exists());
}

#[test]
fn restore_checks_recorded_pair_and_consumes_the_backup() {
    let f = Fixture::new(); let source = f.file("song.wav", b"original"); let backups = f.dir("backups");
    let plan = OutputPlan::new(&selected(&source), SourceCodec::Lpcm, &replacement(&backups, AudioFormat::Wav), None).unwrap();
    let tx = begin(&plan).unwrap(); let backup = tx.backup_path().unwrap().to_owned();
    stage(tx.staging_path(), b"processed"); tx.finish(&Cancellation::new()).unwrap();
    let identity = file_workflow::FileIdentity::capture(&source).unwrap();
    let mark = marker(Some(&backup));
    let other = f.file("backups/unrelated.wav", b"original");
    assert!(matches!(file_workflow::restore(&source, &other, Some(&mark), RestoreStatus::Inactive, &identity, &Cancellation::new()), Err(WorkflowError::BackupMismatch { .. })));
    assert!(matches!(file_workflow::restore(&source, &backup, Some(&mark), RestoreStatus::Active, &identity, &Cancellation::new()), Err(WorkflowError::RestoreBusy(_))));
    assert!(matches!(file_workflow::restore(&source, &backup, Some(&mark), RestoreStatus::Protected, &identity, &Cancellation::new()), Err(WorkflowError::RestoreProtected(_))));
    assert_eq!(fs::read(&source).unwrap(), b"processed");
    let backup_meta = fs::metadata(&backup).unwrap();
    let report = file_workflow::restore(&source, &backup, Some(&mark), RestoreStatus::Inactive, &identity, &Cancellation::new()).unwrap();
    assert_eq!(report.destination, source); assert_eq!(report.backup, backup);
    assert!(report.backup_removed); assert!(report.backup_removal_error.is_none());
    assert_eq!(fs::read(&report.destination).unwrap(), b"original"); assert!(!report.backup.exists());
    let restored_meta = fs::metadata(&report.destination).unwrap();
    assert_eq!(restored_meta.mode(), backup_meta.mode());
    assert_eq!((restored_meta.mtime(), restored_meta.mtime_nsec()), (backup_meta.mtime(), backup_meta.mtime_nsec()));
}

#[test]
fn restore_reports_backup_unlink_failure_without_undoing_restored_file() {
    let f = Fixture::new(); let source = f.file("song.wav", b"original"); let backups = f.dir("backups");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
    let plan = OutputPlan::new(&selected(&source), SourceCodec::Lpcm, &replacement(&backups, AudioFormat::Wav), None).unwrap();
    let tx = begin(&plan).unwrap(); let backup = tx.backup_path().unwrap().to_owned();
    let backup_meta = fs::metadata(&backup).unwrap();
    stage(tx.staging_path(), b"processed"); tx.finish(&Cancellation::new()).unwrap();
    let identity = FileIdentity::capture(&source).unwrap();
    fs::set_permissions(&backups, fs::Permissions::from_mode(0o500)).unwrap();
    let restored = file_workflow::restore(&source, &backup, Some(&marker(Some(&backup))), RestoreStatus::Inactive, &identity, &Cancellation::new());
    fs::set_permissions(&backups, fs::Permissions::from_mode(0o700)).unwrap();
    let report = restored.unwrap();
    assert!(!report.backup_removed);
    let error = report.backup_removal_error.unwrap();
    assert!(!error.message.is_empty());
    assert_eq!(error.retained_path.as_deref(), Some(backup.as_path()));
    assert_eq!(fs::read(&source).unwrap(), b"original"); assert_eq!(fs::read(&backup).unwrap(), b"original");
    let restored_meta = fs::metadata(&source).unwrap();
    assert_eq!(restored_meta.mode(), backup_meta.mode());
    assert_eq!((restored_meta.mtime(), restored_meta.mtime_nsec()), (backup_meta.mtime(), backup_meta.mtime_nsec()));
}

#[test]
fn restore_keeps_a_backup_reserved_by_another_job() {
    let f = Fixture::new();
    let current = f.file("processed.wav", b"processed");
    let backup = f.file("backups/original.wav", b"original");
    let copies = f.dir("other-backups");
    let hold_plan = OutputPlan::new(&selected(&backup), SourceCodec::Lpcm, &replacement(&copies, AudioFormat::Wav), None).unwrap();
    let held = begin(&hold_plan).unwrap();
    let identity = FileIdentity::capture(&current).unwrap();
    let report = file_workflow::restore(&current, &backup, Some(&marker(Some(&backup))),
        RestoreStatus::Inactive, &identity, &Cancellation::new()).unwrap();
    assert!(!report.backup_removed);
    assert_eq!(report.backup_removal_error.unwrap().retained_path.as_deref(), Some(backup.as_path()));
    assert_eq!(fs::read(&current).unwrap(), b"original");
    assert_eq!(fs::read(&backup).unwrap(), b"original");
    held.abort().unwrap();
}

#[test]
fn shared_backup_restores_all_copies_before_consuming_each_authorized_path() {
    for hardlink_alias in [false, true] {
        let f = Fixture::new();
        let first = f.file("first.wav", b"processed first");
        let second = f.file("second.wav", b"processed second");
        let backup = f.file("backups/original.wav", b"original");
        let alias = if hardlink_alias {
            let alias = f.0.join("backups/alias.wav"); fs::hard_link(&backup, &alias).unwrap(); alias
        } else { backup.clone() };
        let marker_a = marker(Some(&backup)); let marker_b = marker(Some(&alias));
        let snapshot = file_workflow::RestoreBatchSnapshot::capture(&[first.clone(), second.clone()], &[backup.clone(), alias.clone()]).unwrap();
        let requests = [
            file_workflow::RestoreRequest { processed: &first, backup: &backup, marker: Some(&marker_a), status: RestoreStatus::Inactive,
                identity: snapshot.target(0), frozen_backup: snapshot.backup(0) },
            file_workflow::RestoreRequest { processed: &second, backup: &alias, marker: Some(&marker_b), status: RestoreStatus::Inactive,
                identity: snapshot.target(1), frozen_backup: snapshot.backup(1) },
        ];
        let results = file_workflow::restore_group(&requests, true, &Cancellation::new());
        assert_eq!(results.len(), 2);
        for result in results {
            let report = result.unwrap();
            assert!(report.backup_removed, "{report:?}");
            assert!(report.backup_removal_error.is_none());
        }
        assert_eq!(fs::read(&first).unwrap(), b"original");
        assert_eq!(fs::read(&second).unwrap(), b"original");
        assert!(!backup.exists()); assert!(!alias.exists());
    }
}

#[test]
fn shared_backup_is_retained_if_any_restore_fails() {
    let f = Fixture::new();
    let first = f.file("first.wav", b"processed first");
    let second = f.file("second.wav", b"processed second");
    let backup = f.file("backups/original.wav", b"original");
    let marker = marker(Some(&backup));
    let snapshot = file_workflow::RestoreBatchSnapshot::capture(&[first.clone(), second.clone()], &[backup.clone(), backup.clone()]).unwrap();
    let requests = [
        file_workflow::RestoreRequest { processed: &first, backup: &backup, marker: Some(&marker), status: RestoreStatus::Inactive,
            identity: snapshot.target(0), frozen_backup: snapshot.backup(0) },
        file_workflow::RestoreRequest { processed: &second, backup: &backup, marker: Some(&marker), status: RestoreStatus::Active,
            identity: snapshot.target(1), frozen_backup: snapshot.backup(1) },
    ];
    let results = file_workflow::restore_group(&requests, true, &Cancellation::new());
    let mut results = results.into_iter();
    let report = results.next().unwrap().unwrap();
    assert!(!report.backup_removed); assert!(report.backup_removal_error.is_some());
    assert!(matches!(results.next().unwrap(), Err(WorkflowError::RestoreBusy(_))));
    assert_eq!(fs::read(&first).unwrap(), b"original");
    assert_eq!(fs::read(&second).unwrap(), b"processed second");
    assert_eq!(fs::read(&backup).unwrap(), b"original");
}

#[test]
fn shared_restore_rejects_a_backup_changed_after_batch_snapshot() {
    let f = Fixture::new();
    let first = f.file("first.wav", b"processed first");
    let second = f.file("second.wav", b"processed second");
    let backup = f.file("backups/original.wav", b"original");
    let marker = marker(Some(&backup));
    let snapshot = file_workflow::RestoreBatchSnapshot::capture(&[first.clone(), second.clone()], &[backup.clone(), backup.clone()]).unwrap();
    assert_eq!(snapshot.groups(), &[vec![0, 1]]);
    let modified = fs::metadata(&backup).unwrap().modified().unwrap();
    let mut file = OpenOptions::new().write(true).open(&backup).unwrap();
    file.write_all(b"tampered").unwrap();
    file.set_times(fs::FileTimes::new().set_modified(modified)).unwrap();
    let requests = [
        file_workflow::RestoreRequest { processed: &first, backup: &backup, marker: Some(&marker), status: RestoreStatus::Inactive,
            identity: snapshot.target(0), frozen_backup: snapshot.backup(0) },
        file_workflow::RestoreRequest { processed: &second, backup: &backup, marker: Some(&marker), status: RestoreStatus::Inactive,
            identity: snapshot.target(1), frozen_backup: snapshot.backup(1) },
    ];
    let results = file_workflow::restore_group(&requests, true, &Cancellation::new());
    assert!(results.into_iter().all(|result| matches!(result, Err(WorkflowError::BackupChanged(_)))));
    assert_eq!(fs::read(first).unwrap(), b"processed first");
    assert_eq!(fs::read(second).unwrap(), b"processed second");
    assert_eq!(fs::read(backup).unwrap(), b"tampered");
}

#[test]
fn restore_distinguishes_suffix_only_missing_backup_and_unprocessed() {
    let f = Fixture::new(); let processed = f.file("suffix_PN.wav", b"processed");
    let identity = file_workflow::FileIdentity::capture(&processed).unwrap();
    let missing = f.0.join("missing.wav");
    assert!(matches!(file_workflow::restore(&processed, &missing, None, RestoreStatus::Inactive, &identity, &Cancellation::new()), Err(WorkflowError::RestoreNotNeeded(_))));
    assert!(matches!(file_workflow::restore(&processed, &missing, Some(&marker(None)), RestoreStatus::Inactive, &identity, &Cancellation::new()), Err(WorkflowError::BackupNotRecorded(_))));
    assert!(matches!(file_workflow::restore(&processed, &missing, Some(&marker(Some(&missing))), RestoreStatus::Inactive, &identity, &Cancellation::new()), Err(WorkflowError::BackupNotFound(_))));
    let plain = f.file("plain.wav", b"plain");
    let identity = file_workflow::FileIdentity::capture(&plain).unwrap();
    assert!(matches!(file_workflow::restore(&plain, &missing, None, RestoreStatus::Inactive, &identity, &Cancellation::new()), Err(WorkflowError::NotProcessed(_))));
    assert_eq!(fs::read(processed).unwrap(), b"processed");
}

#[test]
fn stale_marker_cannot_authorize_restoring_over_a_replaced_file() {
    let f = Fixture::new();
    let processed = f.file("processed.wav", b"processed A");
    let backup = f.file("backup.wav", b"original A");
    let identity = file_workflow::FileIdentity::capture(&processed).unwrap();
    let old_marker = marker(Some(&backup));
    let replacement = f.file("replacement.wav", b"new file B");
    fs::rename(replacement, &processed).unwrap();
    assert!(matches!(file_workflow::restore(&processed, &backup, Some(&old_marker),
        RestoreStatus::Inactive, &identity, &Cancellation::new()), Err(WorkflowError::SourceChanged(_))));
    assert_eq!(fs::read(&processed).unwrap(), b"new file B");
    assert_eq!(fs::read(&backup).unwrap(), b"original A");
}

#[test]
fn concurrent_replacement_jobs_claim_distinct_backup_names_atomically() {
    use std::sync::{Arc, Barrier};
    use std::thread;
    let f = Fixture::new(); let backups = f.dir("backups");
    let first = f.file("a/song.wav", b"first original");
    let second = f.file("b/song.wav", b"second original");
    let settings = replacement(&backups, AudioFormat::Wav);
    let first_plan = OutputPlan::new(&selected(&first), SourceCodec::Lpcm, &settings, None).unwrap();
    let second_plan = OutputPlan::new(&selected(&second), SourceCodec::Lpcm, &settings, None).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let start = |plan: OutputPlan, barrier: Arc<Barrier>| thread::spawn(move || {
        barrier.wait();
        begin(&plan).unwrap()
    });
    let first_job = start(first_plan, barrier.clone());
    let second_job = start(second_plan, barrier);
    let first_tx = first_job.join().unwrap(); let second_tx = second_job.join().unwrap();
    let first_backup = first_tx.backup_path().unwrap().to_owned();
    let second_backup = second_tx.backup_path().unwrap().to_owned();
    assert_ne!(first_backup, second_backup);
    assert_eq!(fs::read(&first_backup).unwrap(), b"first original");
    assert_eq!(fs::read(&second_backup).unwrap(), b"second original");
    let mut names = vec![first_backup, second_backup]; names.sort();
    assert_eq!(names, vec![backups.join("song.wav"), backups.join("song_2.wav")]);
    first_tx.abort().unwrap(); second_tx.abort().unwrap();
}

#[test]
fn backup_skips_a_reserved_candidate_before_its_owner_publishes() {
    let f = Fixture::new(); let target_dir = f.dir("reserved"); let first_backups = f.dir("first-backups");
    let first = f.file("reserved/song.wav", b"first original");
    let first_plan = OutputPlan::new(&selected(&first), SourceCodec::Aac, &replacement(&first_backups, AudioFormat::Wav), None).unwrap();
    let first_tx = begin(&first_plan).unwrap();
    assert_eq!(first_plan.destination(), target_dir.join("song.m4a"));
    assert!(!first_plan.destination().exists());

    let second = f.file("other/song.m4a", b"second original");
    let second_plan = OutputPlan::new(&selected(&second), SourceCodec::Aac, &replacement(&target_dir, AudioFormat::Wav), None).unwrap();
    let second_tx = begin(&second_plan).unwrap();
    assert_eq!(second_tx.backup_path().unwrap(), target_dir.join("song_2.m4a"));
    assert!(!first_plan.destination().exists());
    stage(first_tx.staging_path(), b"first encoded");
    let published = first_tx.finish(&Cancellation::new()).unwrap();
    assert_eq!(fs::read(published.destination).unwrap(), b"first encoded");
    assert_eq!(fs::read(second_tx.backup_path().unwrap()).unwrap(), b"second original");
    second_tx.abort().unwrap();
    assert_eq!(fs::read(second).unwrap(), b"second original");
}

#[test]
fn mirroring_refuses_a_symlinked_output_component() {
    let f = Fixture::new(); let selected_dir = f.dir("input/Album");
    let out = f.dir("output"); let unrelated = f.dir("unrelated");
    f.file("input/Album/song.wav", b"original");
    symlink(&unrelated, out.join("Album")).unwrap();
    let item = file_workflow::discover_inputs(&[selected_dir]).unwrap().remove(0);
    let settings = OutputSettings { mode: OutputMode::SpecificFolder { directory: out, mirror: true }, format: AudioFormat::Wav };
    let plan = OutputPlan::new(&item, SourceCodec::Lpcm, &settings, None).unwrap();
    assert!(matches!(begin(&plan).err().unwrap(), WorkflowError::InvalidPath { .. }));
    assert!(!unrelated.join("song_PN.wav").exists());
}

#[test]
fn restore_rejects_a_backup_that_is_a_hard_link_to_the_current_file() {
    let f = Fixture::new(); let current = f.file("current.wav", b"processed");
    let alias = f.0.join("alias.wav"); fs::hard_link(&current, &alias).unwrap();
    let identity = file_workflow::FileIdentity::capture(&current).unwrap();
    let mark = marker(Some(&alias));
    assert!(matches!(file_workflow::restore(&current, &alias, Some(&mark), RestoreStatus::Inactive, &identity, &Cancellation::new()), Err(WorkflowError::InvalidPath { .. })));
    assert_eq!(fs::read(&current).unwrap(), b"processed");
    assert_eq!(fs::read(&alias).unwrap(), b"processed");
}

#[test]
fn near_name_max_destination_can_be_reserved_without_truncating_it() {
    let f = Fixture::new();
    let basename = format!("{}.wav", "x".repeat(240));
    let source = f.file(&basename, b"original");
    let plan = OutputPlan::new(&selected(&source), SourceCodec::Lpcm, &same_folder(AudioFormat::Wav), None).unwrap();
    let tx = begin(&plan).unwrap();
    assert!(matches!(begin(&plan).err().unwrap(), WorkflowError::ReservationExists(_)));
    stage(tx.staging_path(), b"candidate");
    let published = tx.finish(&Cancellation::new()).unwrap();
    assert_eq!(fs::read(published.destination).unwrap(), b"candidate");
    assert_eq!(fs::read(source).unwrap(), b"original");
}

#[test]
fn captured_identity_is_required_before_creating_a_workspace_or_backup() {
    let f = Fixture::new(); let backups = f.dir("backups");
    let source = f.file("song.wav", b"before");
    let plan = OutputPlan::new(&selected(&source), SourceCodec::Lpcm, &replacement(&backups, AudioFormat::Wav), None).unwrap();
    let identity = FileIdentity::capture(&source).unwrap();
    fs::write(&source, b"different input").unwrap();
    assert!(matches!(plan.begin(&identity), Err(WorkflowError::SourceChanged(_))));
    assert_eq!(fs::read(&source).unwrap(), b"different input");
    assert_eq!(fs::read_dir(backups).unwrap().count(), 0);
}

#[test]
fn cancellation_before_publication_preserves_original_and_restore_input() {
    let f = Fixture::new(); let backups = f.dir("backups"); let source = f.file("song.wav", b"original");
    let plan = OutputPlan::new(&selected(&source), SourceCodec::Lpcm, &replacement(&backups, AudioFormat::Wav), None).unwrap();
    let tx = begin(&plan).unwrap(); let backup = tx.backup_path().unwrap().to_owned();
    let workspace = workspace_directory(&tx, plan.destination().parent().unwrap());
    stage(tx.staging_path(), b"encoded");
    let cancelled = Cancellation::new(); cancelled.cancel();
    let error = tx.finish(&cancelled).unwrap_err();
    assert!(error.is_cancelled());
    assert!(matches!(&error, WorkflowError::Transaction { backup: Some(p), published: false, .. } if p == &backup));
    assert_eq!(fs::read(&source).unwrap(), b"original");
    assert_eq!(fs::read(&backup).unwrap(), b"original");
    assert!(!workspace.exists());

    fs::write(&source, b"processed").unwrap();
    let identity = FileIdentity::capture(&source).unwrap();
    let error = file_workflow::restore(&source, &backup, Some(&marker(Some(&backup))),
        RestoreStatus::Inactive, &identity, &cancelled).unwrap_err();
    assert!(error.is_cancelled());
    assert!(matches!(&error, WorkflowError::Transaction { backup: Some(p), published: false, .. } if p == &backup));
    assert_eq!(fs::read(&source).unwrap(), b"processed");
    assert_eq!(fs::read(&backup).unwrap(), b"original");
}
