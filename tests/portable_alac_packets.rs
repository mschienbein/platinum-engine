//! AudioToolbox golden output for short final packets and for oversized packets of the portable ALAC encoder.
//!
//! The reference encoder scores a packet from an eighth of its frames and trains on a
//! thirty-second, in work buffers it keeps for the whole stream. A packet of fewer than 288
//! frames scores positions beyond the span the last training pass rewrites; they hold that pass's
//! differential head and what the element's own mix search wrote, never anything from an earlier
//! element or packet (`scoring_never_reads_stale_buffers` in `alac.rs`), and the v0.3.0 encoder
//! modelled that span wrongly. The expected digests were recorded from AudioToolbox by
//! `tools/portable/alac/partial_probe.rs` (macOS, 512-frame writes) for the PCM that
//! `tools/portable/alac/partial_fixtures.rs` generates; running them needs no Apple component.
use platinum_engine::portable::audio::alac::{self, Config, EncodeError};
use platinum_engine::portable::audio::{mp4, CodecError, OutputFormat, Writer, WriterConfig};
use platinum_engine::portable::digest::sha256;

#[path = "../tools/portable/alac/partial_fixtures.rs"]
mod fixtures;
use fixtures::{Case, CASES, OVERFLOW};

fn hex(bytes: &[u8]) -> String { sha256(bytes).iter().map(|b| format!("{b:02x}")).collect() }

fn config(case: &Case) -> Config {
    Config { frame_length: 4096, bit_depth: case.depth, pb: 40, mb: 10, kb: 14, channels: case.channels, sample_rate: 44_100 }
}

/// What AudioToolbox writes: one packet per 4096 frames, the remainder last, and an empty
/// packet after an exact multiple.
fn encode(case: &Case, pcm: &[i32]) -> Vec<Vec<u8>> {
    let mut encoder = alac::Encoder::new(config(case)).unwrap();
    let mut packets: Vec<Vec<u8>> = pcm.chunks(4096 * usize::from(case.channels)).map(|c| encoder.encode_packet(c).unwrap()).collect();
    if case.frames % 4096 == 0 { packets.push(encoder.encode_packet(&[]).unwrap()); }
    packets
}

/// (name, packets, last packet bytes, SHA-256 of the length-framed packet stream)
const GOLDEN: &[(&str, usize, usize, &str)] = &[
    ("sparse-2ch-16b-88", 1, 148, "f8db86f2d9c6bf79acf6d2850ada8330ac2560c39e3b6914616e789fd4bc0c41"),
    ("correlated-2ch-20b-4144", 2, 248, "537fe2f407324233fa0ee392b85fcc41aa79de182ba07e79bad300c55bef35dd"),
    ("wave-2ch-32b-18", 1, 119, "fe0056a2ca84882d9aded1d8b9ea9d684fb8ac5b43c002a2044dae597e274dac"),
    ("sparse-3ch-20b-32", 1, 212, "0a5655a68718b339246a7f702f600fb9ae1eacc7e53832fcc8300279b1d2425c"),
    ("noise-3ch-20b-81", 1, 272, "ab12845eb4fa2fb1eed9b07d6e75067d24b33666922e8805c83f45ad6b78adab"),
    ("correlated-3ch-20b-4126", 2, 163, "32d35a3bcfe657784a0d7bc3f6c26cab2efed23510ee5e898f5164ab8d1fb04e"),
    ("noise-3ch-24b-4211", 2, 500, "300f77080897230c75ca46b5973937abd01d22ea0549076e4670df5db49a5514"),
    ("sparse-3ch-32b-135", 1, 1502, "ab38f5e91dc9889aefdad5c32bfdf6d092c74d2bbbf99138148bfe0bdad09ee2"),
    ("noise-4ch-20b-72", 1, 346, "55974e81b7757d87d0b6a7cf0561397a4a71588527849a662e13e239df6771d7"),
    ("noise-4ch-24b-27", 1, 299, "9146fbb877d69cc287a96eda9426fe999ae9af5106d7f17993765edeb61baa2b"),
    ("sparse-5ch-20b-14", 1, 160, "bd475852f3ed414ab8e760086c9779b6289b2245956be06590cad9d88c414382"),
    ("sparse-5ch-24b-8436", 3, 2758, "02e3364cf19826a6e18ebaa2105f895fe2db35465dde94fadbfcb8a65f746c09"),
    ("sparse-5ch-32b-61", 1, 1028, "00f04de9f91704eecf6864e8c67b4903ec5ce171328a498e63aac193c39b032b"),
    ("noise-6ch-16b-56", 1, 565, "c49879737539003c30c5d3ef730b09358b3a56ed88a0780e3cfed41513c05114"),
    ("sparse-6ch-20b-111", 1, 1243, "3583cb0259b7ddf4f3920f20ee6f3ddef62c4224f5fba68e8600a29c52252355"),
    ("noise-6ch-20b-155", 1, 2001, "ebc5a4f585fd8bbf72b5b853674afad9bd16dbe281c56aadbe865e8231582177"),
    ("correlated-6ch-20b-180", 1, 1786, "e6f5dea9dd62f7e659c6a63596d7745ef9fff695f1af0eeb8143936e2032a707"),
    ("correlated-6ch-24b-4136", 2, 748, "49208d066f16b813b3578590a7d3d6c6a97b048607b7200dcd16f4f6df22f779"),
    ("correlated-6ch-24b-8460", 3, 1823, "50cd2c48cdbf01060c29ed70e510213294727ca72cd499f930dba6389d60fe1e"),
    ("sparse-6ch-32b-111", 1, 1993, "3fb9edc316d881c6cbe903fa3c2bf916ee8da44c8a8899d56ce4643d97b29fe9"),
    ("sparse-7ch-16b-209", 1, 1427, "90999c0331ca2a1a4d13f9f2ea59bce845e06e41c8769e9223ff27ba193d031f"),
    ("wave-7ch-20b-4161", 2, 495, "abfd0943b8e77ed6d94542d0f0bdd781a17e471d16ce8ead355e26c8a29bf0bc"),
    ("wave-7ch-24b-4172", 2, 1325, "7a6a05b12ce7626cafe9e337dfc29aab064b7f8a578a5becb8cca115957701e3"),
    ("wave-7ch-32b-15", 1, 405, "ed57d7d527a3b00defe68470409a9efde9d65851bd6df9d5aaa0b6d196b6b0b8"),
    ("wave-7ch-32b-4266", 2, 3832, "daa71fbaa861a887a80fdee9cb8be6b8717004b63af8ce59c62ef1eb7ed9c2ba"),
    ("sparse-8ch-16b-218", 1, 1820, "7c18dd225f7cfe1f7c1fe4e262e23d28706839aca6b56876d4779c1ffba3e710"),
    ("noise-8ch-16b-246", 1, 3107, "722b036935344697b35e668a65ac23f7aa95aaffce3d2659ffc2cfe4e827db26"),
    ("sparse-8ch-16b-4126", 2, 392, "e26d6a19b517e698089eb4b5d70550d2dbef2ff7dc0174ed7853ae0208e0f03a"),
    ("wave-8ch-16b-4187", 2, 635, "dbc6dd7c66513ae5e2a3a6b08958dae67892edfd67eb76f1f6bc4aff4587a069"),
    ("correlated-8ch-16b-4330", 2, 2366, "5d3357d32239c2f7326a3c1fa25a943bdc3753b922d458d17d8abfde585098b6"),
    ("sparse-8ch-20b-83", 1, 1491, "de1dcd54eec9a7c80a5e6498f6afded4c3cc9ce1ab51cd198b984555d667ea61"),
    ("noise-8ch-24b-18", 1, 466, "ed58f5500181b569e693c7bac28886f17008f7c1f913a7e8dffda9d1ad7c0c0f"),
    ("noise-8ch-24b-8363", 3, 1919, "57c022682388ca4ccab7dabec081d0474a2251c15ac0ecdd1d75108d9cb0f349"),
    ("sparse-8ch-32b-4144", 2, 1409, "9c431693b1ae547e6c99aed3ffa297ca21458870ca521f38653f4dd6d3b22475"),
    ("correlated-1ch-24b-8237", 3, 102, "d3b7a8231a95101c6e46af2f76490bb2035bd04858b8217de5c22db0f7fbc9d9"),
    ("sparse-1ch-16b-4197", 2, 129, "a6648cf0845d365281ef73401f7c228fcf1403767fc0e798835b0229de940aa2"),
    ("correlated-2ch-20b-699", 1, 1421, "fce6304ec77be6990354dc2353bfe768004e28ded46d7c30c28783853946fbaa"),
    ("noise-6ch-16b-4775", 2, 4044, "6ded40ae90693371db72bf075751d1db2a6c6187115a96dbb8af9647a5b8a8f6"),
    ("correlated-8ch-32b-4165", 2, 1239, "65eaa233fdfde1421d18274a982e9da402a3712054c7683bbba012085716dc13"),
    ("wave-3ch-20b-8337", 3, 1102, "f9846a72d062414b62bc43d826947c6714677329c4151c3f4684097682646891"),
    ("correlated-2ch-32b-8194", 3, 24, "e5d6ee7d6f5cb26f063507cd72739bbdd939b990d38c21731d79cf0b61ee72d0"),
    ("wave-3ch-24b-8192", 3, 15, "fac8798c93970dadce3bc0ce73aed97c2e475b02bc7328fe4949c9063c05eed9"),
    ("noise-2ch-16b-1", 1, 12, "0b36b0da2e7f6330691eae9f56a92957550be76fa8284ad352889db7e1f28004"),
    ("noise-5ch-16b-7", 1, 91, "e7d54b9a5214d74df0625b056b4469bb471c13922b281e2ffb258d0c4521cd29"),
    ("wave-2ch-16b-8", 1, 40, "7218b81d4a4224f7c6478b43333119a005dbeb13b97889656c37511f535aff5e"),
];

#[test]
fn short_final_packets_match_audiotoolbox() {
    assert_eq!(GOLDEN.len(), CASES.len());
    for (case, &(name, packets, last, digest)) in CASES.iter().zip(GOLDEN) {
        assert_eq!(case.name, name);
        let pcm = fixtures::pcm(case);
        let encoded = encode(case, &pcm);
        let sizes: Vec<usize> = encoded.iter().map(Vec::len).collect();
        assert_eq!(encoded.len(), packets, "{name}: packet sizes {sizes:?}");
        assert_eq!(encoded.last().unwrap().len(), last, "{name}: packet sizes {sizes:?}");
        assert_eq!(hex(&fixtures::framed(&encoded)), digest, "{name}: packet sizes {sizes:?}");
        let mut decoded = Vec::new();
        let mut decoder = alac::Decoder::new();
        for (chunk, packet) in pcm.chunks(4096 * usize::from(case.channels)).zip(&encoded) {
            decoder.decode_packet_into(config(case), packet, &mut decoded).unwrap();
            assert_eq!(decoded, chunk, "{name}: packet does not decode to its input");
        }
    }
}

/// The 5-channel 16-bit shape of the v0.3.0 repro: one full packet, which all of these share,
/// and a final packet of 1 to 40 frames. The previous encoder differed from AudioToolbox for
/// 13 to 15, 21 to 34 and 40 of these frames.
const SERIES_FIRST_PACKET: &str = "475f8924aebbce405bfdc50e6119134bbaef21cce3575f07158c20de5cdd6476";
/// (frames, bytes, SHA-256) of the final packet.
const SERIES: &[(usize, usize, &str)] = &[
    (1, 31, "bf20b807eedc7e34e2435cd12ae415a22722928684e816ecf0e5738a8609aa70"),
    (2, 41, "04305a6126ce0a401929c8b21fe97d8101c2410afcf60f5cdfe51cbc8f9f87de"),
    (3, 51, "4c753f14c4fa3ca4f00cdf0df9d7d11aa51d6fcfcb51fbc423f9b5b44ffc487c"),
    (4, 61, "19c4574d1d7744917ecee4b1de26ec2194541ceac489bc2664947e8dcad3ba0f"),
    (5, 71, "526819b04090a7b85ddcff2885bb500c80d88c98b48d8cba31f6b20bdf4dab19"),
    (6, 81, "405b91ca9ca0f0cefb24a18383140e6e8387e2898a294c6225e9ca4f653582bc"),
    (7, 90, "17ec1e2944647075d3c3074fb07b82792d959f00e5969e43725c23d61b8f9687"),
    (8, 98, "226dc19f8f2740e87e029909907283fe84ec67c230ae67dfa46689736732a156"),
    (9, 106, "064accaa47ed211712000c5e01f01455f9749023ff7afe8cb0c41e236d4ad8a4"),
    (10, 113, "d706a7e8b593ab03f6584c89f41bbbafcec22e6a2fe24088970a089ae7f32afd"),
    (11, 121, "4e77f4fe54f13734b523e4570f00dca13ab66ec98b54dc0ecab4c311ce139223"),
    (12, 132, "d2f914c0e5d3ac61ae09e5b2bd9ac4762eacb8826a44cc8949e3ef75183ab2b7"),
    (13, 136, "3c1c09cc7e4814ea2f663aa28e81d6cef6222e2b47334f5f3f6ddf89095a9ab9"),
    (14, 143, "c6b1c096197a00437436943f27273b7eb520a557c67bcc084eaf1cc14781da5b"),
    (15, 149, "bc538606281c7ab04d196d0e5b22b25dd7575e1e7f5d6172814fa05b6af58462"),
    (16, 162, "72794e615b786973cd422c387706ae2a93c99bbc5f10794bbf5c4ac3fb8e892b"),
    (17, 170, "47f2809e5aa8c96a42723a3ca48687eec73d8639f0ba431ddaa3b78a4a2798f7"),
    (18, 177, "4e3f1a43724adeeba16c7a6af6d4b6ce273aa05ce644ad46b0645e611e3ca622"),
    (19, 183, "4334d581c8bbe3d841c656e6c15ae861bb658bc6f2811b6b9e10201c9fc32bea"),
    (20, 189, "23bdb6c683337856dcac6944c1bd8391327524c577af79ffa8b40b1eda864981"),
    (21, 177, "542b6c4d0bfdfdf79cf18ec67abe3337d401975a41a9fd1c5871ae01b3864892"),
    (22, 180, "d77be91b6e16b7548e584e89bdcaf2aa4b3c16ffd2bdfff9eb80cf2372fc6fd3"),
    (23, 183, "31ebb7ca66ac7ecb4691a19411f44f86cdbb17b53d754c5ca617094efa6f74ff"),
    (24, 185, "57b0cd83ebbea23c3c5ff5b283b5bc5f9c55145b5ba24e7f71a1c534156682cc"),
    (25, 191, "5072f9c1b669001e770df03fa179c4a7eddaecb175f322e8857ebf0c6dee08cf"),
    (26, 196, "f6c39ee71523df1ca5d18fd7de2c9f74535d5e3dcab313350d59055fb362139e"),
    (27, 201, "80a317ca4d511e558b8a63f789b9dafd324bb3a0e5824da299cb96403d3e3ecc"),
    (28, 206, "5f8648408dc7d913706bba4fbd05e40cd7f624cac343cc67ec01a0ca8587c855"),
    (29, 209, "682bc8ae1e178dc068732447566db4c7f214e1938dcc6e4f418fdd037b172bdc"),
    (30, 214, "64b52003c6b7c63666cda0209e104f9fbc8101ae5c11f4ab95e7801664ded5e3"),
    (31, 220, "02058c096e991ed4800fd9a8de41a396ba8382f095999f63e187aabaec8df806"),
    (32, 225, "6d6fb0a12e6b0849235f57beaaeca7345eb2b8e1bd401523f1c3c86b75bf2539"),
    (33, 231, "3dc999db61974a9b38b8082c43c29a103968f1da3eab1a1a7d553260704a566a"),
    (34, 236, "36c2f1942a6af3ec14fea9676e63c4da5d96091a607f69e0815ea15dc269ca15"),
    (35, 239, "17855b6f306db88be0b327f0aecf0c8849a816f917a198350c47124150879fa5"),
    (36, 243, "c1eb6882b4a07e21cbc964ac08ef0759da24f1e7b3e8b0954dbe4d7325737154"),
    (37, 246, "43044e7fa27f80708e39fe2677cbce6352be44914f3d7067f5240a42c74c8865"),
    (38, 249, "99747bf48a9049c209a74c44a14579c5d993dd29a1881aeb6a220a1c74a702d1"),
    (39, 253, "b43dad56d6aa0275c99712e05e42b5e3e613fa4c76968fda0b1f3bf315d2491b"),
    (40, 258, "7a2353d2414bd42d6db45220910df7e5636df3946befd9fdd2615df6d98a7061"),
];

#[test]
fn final_packet_series_matches_audiotoolbox() {
    assert_eq!(SERIES.len(), fixtures::SERIES_SIZES.count());
    for (size, &(expected_size, bytes, digest)) in fixtures::SERIES_SIZES.zip(SERIES) {
        assert_eq!(size, expected_size);
        let case = fixtures::series_case(size);
        let packets = encode(&case, &fixtures::pcm(&case));
        assert_eq!(packets.len(), 2);
        assert_eq!(hex(&packets[0]), SERIES_FIRST_PACKET, "first packet before a {size}-frame packet");
        assert_eq!(packets[1].len(), bytes, "final packet of {size} frames");
        assert_eq!(hex(&packets[1]), digest, "final packet of {size} frames");
    }
}

/// An oversized packet (larger than AudioToolbox's output buffer, which happens to near-incompressible multichannel
/// audio at 24 or 32 bits and to 8-channel 16-bit audio) is not written: AudioToolbox encodes it again with the predictor
/// state of the failed attempt, once per later write call and once at close, and its cookie counts every attempt. The
/// cases are noise just above the level where the first attempt overflows, some with NaN and infinities, written in the
/// engine's 512-frame calls; the expected values are AudioToolbox's file (packets, cookie `maxFrameBytes` and
/// `avgBitRate`, SHA-256 of the whole file with its clock zeroed) or `None` where the macOS writer refuses the result
/// (a packet that never fits, or a file that decodes to fewer frames than were written).
const OVERFLOW_GOLDEN: &[(&str, Option<(usize, u32, u32, &str)>)] = &[
    ("ovf-4ch-32b-4096-f1", Some((2, 65545, 3706754, "3f6e1b348df8b83e00d5224f381f3e58678aa55fad9fa7c667c1fe6f51b711ad"))),
    ("ovf-4ch-32b-4109-f2nan", Some((2, 65545, 4232135, "d6ff07a874f9375e74b84dd9a73c0629249e0af5de88e47459998cf4be691092"))),
    ("ovf-4ch-32b-4096-limit", None),
    ("ovf-4ch-32b-5120-loss", None),
    ("ovf-4ch-32b-8205-multi", Some((3, 65545, 4697755, "0fcbc033c8dbdae664e3d437f2761591340086e40610072f095e3eeebca84022"))),
    ("ovf-4ch-32b-8205-ctl", Some((3, 63570, 3640030, "ef9a6dcc72bf678fe29d437c760963043965fcbe22e5fab27ae1ce77fbf35419"))),
    ("ovf-5ch-32b-4096-f1", Some((2, 81929, 5061812, "d543943441d94046e0f13026eae66f3d54c4796be0e2460b45ab39f0b8e71f0f"))),
    ("ovf-5ch-32b-4109-f2nan", Some((2, 81929, 5760023, "e70079838816c403182edab87b855023101110f005830cb603340840f61fb92b"))),
    ("ovf-5ch-32b-4096-limit", None),
    ("ovf-5ch-32b-5120-loss", None),
    ("ovf-5ch-32b-12301-multi", Some((4, 81929, 6574861, "b0a5d8bc740d5995f2849498f0179db0b6d397928cb2839d6fd838f6531afb15"))),
    ("ovf-5ch-32b-8205-ctl", Some((3, 79442, 4949312, "87cf206c4754a55f529a6437c8ac91c523a88eb94b4a101a89575a170e285bef"))),
    ("ovf-6ch-24b-4096-f1", Some((2, 73740, 9113125, "f40db36ea6af1915dd40cc647cd838b6904fe6d57049fb11e57a3a5b6d65378e"))),
    ("ovf-6ch-24b-4109-f2nan", Some((2, 73740, 10344656, "0cc6bca4d2caf3b15c790380c1e28277f6f00b541920ed2edbe799796d0f2d50"))),
    ("ovf-6ch-24b-4096-limit", None),
    ("ovf-6ch-24b-5120-loss", None),
    ("ovf-6ch-24b-8205-multi", Some((3, 73740, 11508562, "0e1085376fbb56f886f1445f5da3e4ffedf8b8f55fcc856f23b2177ba64c4e47"))),
    ("ovf-6ch-24b-8205-ctl", Some((3, 71280, 8840250, "40880946eaed6a93870f9c0585631eca01bd78f86f413bdc28ec0afdcd66a57d"))),
    ("ovf-8ch-16b-4096-f1", Some((2, 65551, 1838648, "999518d8858be24a4e15093a6204dc591097b263894fc1ffb0b00eddde9f6a41"))),
    ("ovf-8ch-16b-4109-f2nan", Some((2, 65551, 2111653, "c4c7bae836308018e06f8f7aecaa2cc6bfbfc49ba201122479ce2293656d4fc1"))),
    ("ovf-8ch-16b-4096-limit", None),
    ("ovf-8ch-16b-5120-loss", None),
    ("ovf-8ch-16b-8192-multi", Some((3, 65551, 2345618, "699426298526bacc33eada8543dc37ab51a3e9a6577b1d82500ef453a7ed23a1"))),
    ("ovf-8ch-16b-8205-ctl", Some((3, 61588, 1753520, "72333e3134d8a8711a76dad9967632c01da02d03f504bc018d8594bc093836e9"))),
    ("ovf-7ch-24b-4096-f1", Some((2, 86031, 9752244, "10548d98c912c9b08ad98cd12f10091d3bd89555fb288ac5ece4b42e236679f5"))),
    ("ovf-7ch-24b-4109-f2nan", Some((2, 86031, 11094896, "f157e836a70ecef4db13902c110e1eaf1be132a90c98342c28dd5f538311aec6"))),
    ("ovf-7ch-24b-4096-limit", None),
    ("ovf-7ch-24b-5120-loss", None),
    ("ovf-7ch-24b-8192-multi", Some((3, 86031, 12332323, "80a1d79d92c850906ef7d5e6ddedac1a79507584f2e9bb24f104553ce350e136"))),
    ("ovf-7ch-24b-8205-ctl", Some((3, 82596, 9441878, "16795dc2fa48f3dd14de1c81ad5cfd8f3732415092417b49081b46b3fe7a2749"))),
    ("ovf-6ch-32b-4096-f1", Some((2, 98316, 5597541, "a159c491f3fe77896f4771abad244dcd97354fda2dc037c9e6261c0b6f41aee1"))),
    ("ovf-6ch-32b-4109-f2nan", Some((2, 98316, 6341356, "8891c4f457e542a5692ef36785fbc0f8f49ea4752be348433e39e05caf8b101c"))),
    ("ovf-6ch-32b-4096-limit", None),
    ("ovf-6ch-32b-5120-loss", None),
    ("ovf-6ch-32b-8205-multi", Some((3, 98316, 7051865, "c3d580f4519e4f213ef4d20faff9e6b41860af99c0d3220c3ba3dafb77e23b59"))),
    ("ovf-6ch-32b-8205-ctl", Some((3, 95857, 5474458, "65b2146ac00d31d7ed5aafdbab488051ef69ad2e992b4f94cc626cc757aa106c"))),
    ("ovf-8ch-24b-4096-f1", Some((2, 98319, 5560102, "43b7a2158f42eee30923d8aff2ddfcbf6e2fd38e11837879fbae1c8c73976c63"))),
    ("ovf-8ch-24b-4109-f2nan", Some((2, 98319, 6342346, "c19fd7335439ef51c080cb242a9b47d781eb21406c65c344c54122846aa52610"))),
    ("ovf-8ch-24b-4096-limit", None),
    ("ovf-8ch-24b-5120-loss", None),
    ("ovf-8ch-24b-8192-multi", Some((3, 98319, 7037237, "3bb7321f7e769c0b21794ed11cdfe193ff94f99a8ba47e8ec4ba8a8abc87780b"))),
    ("ovf-8ch-24b-8205-ctl", Some((3, 94357, 5391684, "9e8adaed50a24ca1f73f60a90ae5dad3f0174ba604573233268ed46f7ea2b086"))),
    ("ovf-8ch-32b-4096-f1", Some((2, 131087, 8099843, "140d583493d93a71a8c3ce1b9fe2080c3811cca97bcd94f9cf7703001111a970"))),
    ("ovf-8ch-32b-4109-f2nan", Some((2, 131087, 9209671, "d605e84fbc9b495a352e4c828e9a0f65d48e520d674fbadfe551ff0786437a96"))),
    ("ovf-8ch-32b-4096-limit", None),
    ("ovf-8ch-32b-5120-loss", None),
    ("ovf-8ch-32b-8192-multi", Some((3, 131087, 10219562, "0178936e19c3cc45f69e04a7c6519c83dd3ffb7d5d4809d98541a9080ab62cac"))),
    ("ovf-8ch-32b-8205-ctl", Some((3, 127125, 7919750, "67ba70cfba601fed1acfc9fc261697f77ab877ab868a08d3d9dfecbd117fda33"))),
];

fn cookie(bytes: &[u8]) -> (u32, u32) {
    let entry = bytes.windows(4).enumerate().filter(|(_, w)| *w == b"alac").map(|(i, _)| i).nth(1).unwrap();
    let config = &bytes[entry + 8..];
    (u32::from_be_bytes(config[12..16].try_into().unwrap()), u32::from_be_bytes(config[16..20].try_into().unwrap()))
}

#[test]
fn oversized_packets_match_audiotoolbox_files() {
    assert_eq!(OVERFLOW_GOLDEN.len(), OVERFLOW.len());
    let dir = std::env::temp_dir().join(format!("pn-alac-oversized-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (mut written, mut refused) = (0, 0);
    for (case, &(name, expected)) in OVERFLOW.iter().zip(OVERFLOW_GOLDEN) {
        assert_eq!(case.name, name);
        let floats = fixtures::overflow_floats(case);
        let path = dir.join(format!("{name}.m4a"));
        let _ = std::fs::remove_file(&path);
        let config = WriterConfig { format: OutputFormat::Alac, sample_rate: f64::from(case.rate), channels: u32::from(case.channels), bit_depth: u32::from(case.depth) };
        let mut writer = Writer::create(&path, config).unwrap();
        let mut failure = None;
        for call in floats.chunks(usize::from(case.channels) * 512) {
            if let Err(error) = writer.write(call) { failure = Some(error); break; }
        }
        let outcome = match failure {
            Some(error) => { let _ = writer.abort(); Err(error) }
            None => writer.finish(),
        };
        match (outcome, expected) {
            (Ok(()), Some((packets, max, average, digest))) => {
                let bytes = std::fs::read(&path).unwrap();
                std::fs::remove_file(&path).unwrap();
                let index = mp4::read_alac(&mut std::io::Cursor::new(bytes.clone())).unwrap();
                assert_eq!((index.packets.len(), cookie(&bytes)), (packets, (max, average)), "{name}");
                assert_eq!(hex(&fixtures::mask_times(bytes)), digest, "{name}");
                written += 1;
            }
            (Err(error), None) => {
                assert!(!path.exists(), "{name}: a refused stream publishes nothing");
                match &error {
                    CodecError::AlacEncode(EncodeError::NativePacketLimit { .. }) => assert!(name.ends_with("limit"), "{name}: {error}"),
                    CodecError::AlacEncode(EncodeError::NativeFrameLoss { .. }) => assert!(name.ends_with("loss"), "{name}: {error}"),
                    other => panic!("{name}: unexpected {other}"),
                }
                refused += 1;
            }
            (outcome, expected) => panic!("{name}: portable {outcome:?}, AudioToolbox {expected:?}"),
        }
    }
    assert_eq!((written, refused), (OVERFLOW.len() * 4 / 6, OVERFLOW.len() * 2 / 6));
    std::fs::remove_dir_all(&dir).unwrap();
}
