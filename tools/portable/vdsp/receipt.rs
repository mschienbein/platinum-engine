// Included by the macOS-only portable_vdsp integration test.
fn vector_compare(routine: &str, cases: usize, len: usize, fz: bool) -> (usize, usize, Vec<String>) {
    let pair: Option<(NativeBinary, PortableBinary)> = match routine {
        "vadd" => Some((system_vadd, vdsp::vDSP_vadd)),
        "vsub" => Some((system_vsub, vdsp::vDSP_vsub)),
        "vmul" => Some((system_vmul, vdsp::vDSP_vmul)),
        "vdiv" => Some((system_vdiv, vdsp::vDSP_vdiv)),
        "vmax" => Some((system_vmax, vdsp::vDSP_vmax)),
        "vsmul" | "vabs" | "zvabs" | "zvphas" | "maxvi" | "rmsqv" | "ctoz" | "ztoc" => None,
        _ => panic!("unknown VDSP_ROUTINE"),
    };
    assert!(len <= 32768);
    if routine == "rmsqv" { assert_eq!(len, 4096); }
    let run = || {
        let fpcr: u64;
        unsafe { std::arch::asm!("mrs {value}, fpcr", value = out(reg) fpcr, options(nostack, preserves_flags)); }
        assert_eq!(fpcr & (1 << 24) != 0, fz, "unexpected FPCR.FZ");
        let mut random = Random(0x8a7d_5b31_cc27_a601);
        let mut a = vec![0.0f32; len];
        let mut b = vec![0.0f32; len];
        let mut interleaved = vec![0.0f32; len * 2];
        let mut expected = vec![0.0f32; len * 2 + 8];
        let mut actual = vec![0.0f32; len * 2 + 8];
        let mut expected_im = vec![0.0f32; len + 1];
        let mut actual_im = vec![0.0f32; len + 1];
        let ref_align = ((16 - expected.as_ptr() as usize % 16) % 16) / 4;
        let own_align = ((16 - actual.as_ptr() as usize % 16) % 16) / 4;
        let mut mismatched_vectors = 0usize;
        let mut mismatched_samples = 0usize;
        let mut examples = Vec::new();
        for case in 0..cases {
            for i in 0..len {
                let av = match case % 4 {
                    0 | 3 => f32::from_bits(random.next()),
                    1 => random.audio(),
                    _ => f32::from_bits(random.next() & 0x807f_ffff),
                };
                let bv = if case % 4 == 1 || case % 4 == 2 { random.audio() } else { f32::from_bits(random.next()) };
                a[i] = av;
                b[i] = bv;
                interleaved[2 * i] = av;
                interleaved[2 * i + 1] = bv;
            }
            let factor = if case % 4 == 1 { random.audio() } else { f32::from_bits(random.next()) };
            let alias = case % 3;
            let mut expected_index = 0usize;
            let mut actual_index = 0usize;
            let (reference_base, own_base, count) = if routine == "zvphas" {
                (ref_align, own_align, len)
            } else if routine == "ztoc" {
                (0, 0, len * 2)
            } else if routine == "maxvi" || routine == "rmsqv" {
                (0, 0, 1)
            } else {
                (0, 0, len)
            };
            unsafe {
                if let Some((native, portable)) = pair {
                    if alias == 1 {
                        expected[..len].copy_from_slice(&a);
                        actual[..len].copy_from_slice(&a);
                    } else if alias == 2 {
                        expected[..len].copy_from_slice(&b);
                        actual[..len].copy_from_slice(&b);
                    }
                    let ref_a = if alias == 1 { expected.as_ptr() } else { a.as_ptr() };
                    let own_a = if alias == 1 { actual.as_ptr() } else { a.as_ptr() };
                    let ref_b = if alias == 2 { expected.as_ptr() } else { b.as_ptr() };
                    let own_b = if alias == 2 { actual.as_ptr() } else { b.as_ptr() };
                    native(ref_a, 1, ref_b, 1, expected.as_mut_ptr(), 1, len);
                    portable(own_a, 1, own_b, 1, actual.as_mut_ptr(), 1, len);
                } else {
                    match routine {
                        "vsmul" | "vabs" => {
                            if alias != 0 {
                                expected[..len].copy_from_slice(&a);
                                actual[..len].copy_from_slice(&a);
                            }
                            let ri = if alias != 0 { expected.as_ptr() } else { a.as_ptr() };
                            let oi = if alias != 0 { actual.as_ptr() } else { a.as_ptr() };
                            if routine == "vsmul" {
                                system_vsmul(ri, 1, &factor, expected.as_mut_ptr(), 1, len);
                                vdsp::vDSP_vsmul(oi, 1, &factor, actual.as_mut_ptr(), 1, len);
                            } else {
                                system_vabs(ri, 1, expected.as_mut_ptr(), 1, len);
                                vdsp::vDSP_vabs(oi, 1, actual.as_mut_ptr(), 1, len);
                            }
                        }
                        "zvabs" | "zvphas" => {
                            if alias != 0 && routine == "zvabs" {
                                expected[..len].copy_from_slice(&a);
                                actual[..len].copy_from_slice(&a);
                            }
                            let rr = if alias != 0 && routine == "zvabs" { expected.as_mut_ptr() } else { a.as_mut_ptr() };
                            let own_real = if alias != 0 && routine == "zvabs" { actual.as_mut_ptr() } else { a.as_mut_ptr() };
                            let reference_split = SplitComplex { real: rr, imaginary: b.as_mut_ptr() };
                            let own_split = SplitComplex { real: own_real, imaginary: b.as_mut_ptr() };
                            if routine == "zvabs" {
                                system_zvabs(&reference_split, 1, expected.as_mut_ptr(), 1, len);
                                vdsp::vDSP_zvabs(&own_split, 1, actual.as_mut_ptr(), 1, len);
                            } else {
                                system_zvphas(&reference_split, 1, expected.as_mut_ptr().add(ref_align), 1, len);
                                vdsp::vDSP_zvphas(&own_split, 1, actual.as_mut_ptr().add(own_align), 1, len);
                            }
                        }
                        "maxvi" => {
                            system_maxvi(a.as_ptr(), 1, expected.as_mut_ptr(), &mut expected_index, len);
                            vdsp::vDSP_maxvi(a.as_ptr(), 1, actual.as_mut_ptr(), &mut actual_index, len);
                        }
                        "rmsqv" => {
                            system_rmsqv(a.as_ptr(), 1, expected.as_mut_ptr(), len);
                            vdsp::vDSP_rmsqv(a.as_ptr(), 1, actual.as_mut_ptr(), len);
                        }
                        "ctoz" => {
                            let mut reference = SplitComplex { real: expected.as_mut_ptr(), imaginary: expected_im.as_mut_ptr() };
                            let mut own = SplitComplex { real: actual.as_mut_ptr(), imaginary: actual_im.as_mut_ptr() };
                            system_ctoz(interleaved.as_ptr(), 2, &mut reference, 1, len);
                            vdsp::vDSP_ctoz(interleaved.as_ptr(), 2, &mut own, 1, len);
                        }
                        "ztoc" => {
                            let input = SplitComplex { real: a.as_mut_ptr(), imaginary: b.as_mut_ptr() };
                            system_ztoc(&input, 1, expected.as_mut_ptr(), 2, len);
                            vdsp::vDSP_ztoc(&input, 1, actual.as_mut_ptr(), 2, len);
                        }
                        _ => unreachable!(),
                    }
                }
            }
            let mut vector_diff = expected_index != actual_index;
            for i in 0..count {
                let reference = expected[reference_base + i].to_bits();
                let portable = actual[own_base + i].to_bits();
                let imaginary_diff = routine == "ctoz" && expected_im[i].to_bits() != actual_im[i].to_bits();
                if reference != portable || imaginary_diff {
                    mismatched_samples += 1;
                    vector_diff = true;
                    if examples.len() < 12 {
                        examples.push(format!("case={case} index={i} reference={reference:08x} portable={portable:08x} reference_im={:08x} portable_im={:08x} factor={:08x}",
                            if i < expected_im.len() { expected_im[i].to_bits() } else { 0 },
                            if i < actual_im.len() { actual_im[i].to_bits() } else { 0 }, factor.to_bits()));
                    }
                }
            }
            if vector_diff {
                mismatched_vectors += 1;
                if expected_index != actual_index && examples.len() < 12 {
                    examples.push(format!("case={case} reference_index={expected_index} portable_index={actual_index}"));
                }
            }
        }
        (mismatched_vectors, mismatched_samples, examples)
    };
    if fz { platinum_engine::fp_mode::with_flush_to_zero(run) } else { run() }
}

fn write_receipt_identity(receipt: &mut std::fs::File) {
    use std::io::Write;
    let executable = std::env::current_exe().unwrap();
    let fingerprint = std::process::Command::new("/usr/bin/shasum")
        .args(["-a", "256"]).arg(&executable).output().unwrap();
    assert!(fingerprint.status.success());
    // These are the bytes compiled into this test, not later disk contents.
    // Each source is framed by its little-endian u64 length.
    let sources: [&[u8]; 3] = [
        include_bytes!("../../../src/portable/vdsp.rs"),
        include_bytes!("../../../src/portable/math.rs"),
        include_bytes!("receipt.rs"),
    ];
    let mut hash = std::process::Command::new("/usr/bin/shasum")
        .args(["-a", "256"]).stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped()).spawn().unwrap();
    {
        let mut stdin = hash.stdin.take().unwrap();
        for source in sources {
            stdin.write_all(&(source.len() as u64).to_le_bytes()).unwrap();
            stdin.write_all(source).unwrap();
        }
    }
    let source_hash = hash.wait_with_output().unwrap();
    assert!(source_hash.status.success());
    writeln!(receipt, "macos=27.2 build=26B123 libvDSP_uuid=79FFEC1C-BD65-3324-907A-7FEFC636A526").unwrap();
    writeln!(receipt, "binary_sha256={}", String::from_utf8_lossy(&fingerprint.stdout).trim()).unwrap();
    writeln!(receipt, "compiled_source_bundle_sha256={} framing=length_u64_le_then_bytes order=vdsp.rs,math.rs,receipt.rs",
             String::from_utf8_lossy(&source_hash.stdout).trim()).unwrap();
}

#[test]
#[ignore = "long-running comparison; set VDSP_ROUTINE, VDSP_CASES, VDSP_LENGTH, VDSP_FZ and VDSP_RECEIPT"]
fn vector_receipt() {
    use std::io::Write;
    let routine = std::env::var("VDSP_ROUTINE").expect("VDSP_ROUTINE");
    let cases: usize = std::env::var("VDSP_CASES").expect("VDSP_CASES").parse().unwrap();
    let len: usize = std::env::var("VDSP_LENGTH").expect("VDSP_LENGTH").parse().unwrap();
    let fz = std::env::var("VDSP_FZ").expect("VDSP_FZ") == "1";
    let mut receipt = std::fs::OpenOptions::new().write(true).create_new(true)
        .open(std::env::var_os("VDSP_RECEIPT").expect("VDSP_RECEIPT")).unwrap();
    let strides = match routine.as_str() { "ctoz" => "2,1", "ztoc" => "1,2", _ => "1" };
    let aliases = if matches!(routine.as_str(), "vadd" | "vsub" | "vmul" | "vdiv" | "vmax") {
        "disjoint,input-a,input-b"
    } else if matches!(routine.as_str(), "vsmul" | "vabs" | "zvabs") {
        "disjoint,input"
    } else { "disjoint" };
    writeln!(receipt, "routine=vDSP_{routine} size={len} strides={strides} fz={} seed=0x8a7d5b31cc27a601 requested_vectors={cases} classes=raw-bits,audio,subnormal-mixed,raw-bits alias={aliases} phase_output_alignment=16", u8::from(fz)).unwrap();
    write_receipt_identity(&mut receipt);
    writeln!(receipt, "command={}", std::env::var("VDSP_COMMAND").unwrap_or_default()).unwrap();
    receipt.flush().unwrap();
    let start = std::time::Instant::now();
    let (vectors, samples, examples) = vector_compare(&routine, cases, len, fz);
    writeln!(receipt, "tested_vectors={cases} mismatched_vectors={vectors} mismatched_samples={samples} elapsed_seconds={:.3}", start.elapsed().as_secs_f64()).unwrap();
    for example in examples { writeln!(receipt, "mismatch {example}").unwrap(); }
    receipt.flush().unwrap();
    assert_eq!(vectors, 0);
}

#[test]
#[ignore = "full elementwise length grid; set VDSP_GRID_RECEIPT to a new file"]
fn vector_element_grid() {
    use std::collections::BTreeSet;
    use std::io::Write;
    let mut receipt = std::fs::OpenOptions::new().write(true).create_new(true)
        .open(std::env::var_os("VDSP_GRID_RECEIPT").expect("VDSP_GRID_RECEIPT")).unwrap();
    write_receipt_identity(&mut receipt);
    writeln!(receipt, "command={}", std::env::var("VDSP_COMMAND").unwrap_or_default()).unwrap();
    writeln!(receipt, "seed=0x8a7d5b31cc27a601 cases_per_shape=100 classes=raw-bits,audio,subnormal-mixed,raw-bits aliases=disjoint,input-a,input-b where permitted phase_output_alignment=16").unwrap();
    let mut lengths = BTreeSet::new();
    lengths.extend(0usize..=80);
    lengths.extend([127,128,129,255,256,257,511,512,513,1023,1024,1025,2047,2048,2049,
                    4095,4096,4097,8191,8192,8193,16383,16384,16385,32768]);
    for (lo, hi) in [(248,263),(497,527),(994,1054),(1989,2108),(3979,4216),(20416,20544)] {
        lengths.extend(lo..=hi);
    }
    let start = std::time::Instant::now();
    let mut jobs = 0usize;
    let mut failures = 0usize;
    for routine in ["vadd","vsub","vmul","vdiv","vmax","vsmul","vabs","zvabs","zvphas","ctoz","ztoc"] {
        for &len in &lengths {
            for fz in [false, true] {
                let (vectors, samples, examples) = vector_compare(routine, 100, len, fz);
                jobs += 1;
                failures += usize::from(vectors != 0);
                writeln!(receipt, "routine={routine} length={len} fz={} tested_vectors=100 mismatched_vectors={vectors} mismatched_samples={samples}", u8::from(fz)).unwrap();
                for example in examples { writeln!(receipt, "mismatch {example}").unwrap(); }
            }
        }
    }
    writeln!(receipt, "lengths={} jobs={jobs} failures={failures} elapsed_seconds={:.3}", lengths.len(), start.elapsed().as_secs_f64()).unwrap();
    receipt.flush().unwrap();
    assert_eq!(failures, 0);
}

#[test]
#[ignore = "sequential receipt batch; set VDSP_ROUTINES, VDSP_LENGTH, VDSP_CASES and VDSP_BATCH"]
fn vector_receipt_batch() {
    use std::io::Write;
    let routines = std::env::var("VDSP_ROUTINES").expect("VDSP_ROUTINES");
    let length = std::env::var("VDSP_LENGTH").expect("VDSP_LENGTH");
    let cases = std::env::var("VDSP_CASES").expect("VDSP_CASES");
    let batch = std::env::var("VDSP_BATCH").expect("VDSP_BATCH");
    assert!(!batch.is_empty() && batch.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
    let dir = std::path::Path::new("local-artifacts/portable-parity/vdsp-01");
    let mut index = std::fs::OpenOptions::new().write(true).create_new(true)
        .open(dir.join(format!("batch-{batch}.txt"))).unwrap();
    let executable = std::env::current_exe().unwrap();
    let mut failures = 0;
    for routine in routines.split(',') {
        assert!(!routine.is_empty() && routine.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'));
        for fz in ["0", "1"] {
            let path = dir.join(format!("{routine}-n{length}-fz{fz}-{batch}.txt"));
            let command = format!("{} --ignored --exact system::vector_receipt --nocapture; VDSP_ROUTINE={routine} VDSP_CASES={cases} VDSP_LENGTH={length} VDSP_FZ={fz}",
                                  executable.display());
            let result = std::process::Command::new(&executable)
                .args(["--ignored", "--exact", "system::vector_receipt", "--nocapture"])
                .env("VDSP_ROUTINE", routine).env("VDSP_CASES", &cases)
                .env("VDSP_LENGTH", &length).env("VDSP_FZ", fz)
                .env("VDSP_RECEIPT", &path).env("VDSP_COMMAND", command)
                .output().unwrap();
            writeln!(index, "routine={routine} length={length} fz={fz} exit={:?} receipt={}",
                     result.status.code(), path.display()).unwrap();
            index.flush().unwrap();
            if !result.status.success() {
                failures += 1;
                writeln!(index, "stdout:\n{}\nstderr:\n{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr)).unwrap();
                index.flush().unwrap();
            }
        }
    }
    assert_eq!(failures, 0, "one or more vector receipt runs failed; see batch index");
}
