#!/usr/bin/env python3
"""Generates the structural-damage sweeps the container rules of the AAC reader were recorded from.

    structure_sweeps.py FIXTURE_DIR OUT_DIR

`FIXTURE_DIR` is `tests/fixtures/aac-reader`. Every set is a directory `OUT_DIR/<set>/` of small files and a list `<set>.list` (`name<TAB>path`) for the
two reader probes (`apple/` records AudioToolbox's answer, `portable/` the port's; `compare.py` diffs the TSVs). The sets, and the rules documented in
`docs/AAC_INPUT.md` that they back:

  caf_cut      every prefix of the CAF around its chunk boundaries                     (chunks past the end are ignored, pakt without data)
  caf_data     `data` chunks of every size, complete, cut, or with trailing chunks     (the last packet must start inside a complete chunk)
  caf_chunks   every subset of {kuki chan info pakt free zzzz} with / without data     (no data: no packets, a complete pakt without data fails)
  caf_desc     `desc` fields: frames/bytes per packet, bits, channels, flags, formats  (client format statuses, fpp 0, desc format vs cookie)
  caf_rates    `desc` rates from 0.5 Hz to 3,000,001 Hz and the non-finite ones        (the converter's rate limits)
  adts_header  every header field of one frame (0, 1, 5) set to every value           (profile 3, sampling index 13..15, protection)
  adts_crc     CRC / no CRC frame mixes                                                (the header size of the first frame rules)
  adts_sbr     SBR fill elements in the first or a later access unit                   (HE-AAC in ADTS)
  loas_cut     first frame lengths beyond the end, prefixes                            (a cut first frame opens with no packets)
  mp4_atoms    ftyp / moov sizes, 64-bit sizes                                         (typ? / dta?, a wide moov runs to the end)
  names        non-Ogg content under .ogg .oga .opus .sd2 and other names             (name commitments)
"""
import itertools
import os
import struct
import sys

fixtures, out = sys.argv[1], sys.argv[2]


def read(name):
    return open(os.path.join(fixtures, name), 'rb').read()


caf, m4a, adts, loas = (read(f'tiny-44100-2.{e}') for e in ('caf', 'm4a', 'adts', 'loas'))


class Set:
    def __init__(self, name):
        self.dir = os.path.join(out, name)
        os.makedirs(self.dir, exist_ok=True)
        self.name, self.lines = name, []

    def add(self, name, data):
        path = os.path.join(self.dir, name)
        open(path, 'wb').write(data)
        self.lines.append(f'{name}\t{path}')

    def close(self):
        open(os.path.join(self.dir, f'{self.name}.list'), 'w').write('\n'.join(self.lines) + '\n')


def chunks(data):
    result, pos = [], 8
    while pos + 12 <= len(data):
        kind, size = struct.unpack('>4sq', data[pos:pos + 12])
        result.append((kind, data[pos + 12:pos + 12 + size]))
        pos += 12 + size
    return result


def join(parts):
    return b'caff\x00\x01\x00\x00' + b''.join(kind + struct.pack('>q', len(body)) + body for kind, body in parts)


def with_desc(parts, offset, value, fmt='>I'):
    return [(k, b[:offset] + struct.pack(fmt, value) + b[offset + struct.calcsize(fmt):] if k == b'desc' else b) for k, b in parts]


parts = chunks(caf)
body = dict(parts)

# caf_cut
s = Set('caf_cut')
for n in list(range(0, 141)) + list(range(141, 270, 3)) + list(range(4060, 4110)) + [4200, 5000, 7000, len(caf)]:
    s.add(f't{n:05d}.caf', caf[:n])
s.close()

# caf_data: complete chunks of S bytes, cut chunks (declared size kept), unknown size, followed by a free chunk
s = Set('caf_data')
head = b'caff\x00\x01\x00\x00' + b''.join(k + struct.pack('>q', len(b)) + b for k, b in parts if k not in (b'data', b'free'))
data = body[b'data']
for size in [0, 1, 2, 3, 4, 5, 8, 16, 100, 1000, 2000, 2770, 2771, 2774, 2775, 2776, 3000, 3008, 3009, 3012, 3013]:
    s.add(f'complete_{size:04d}.caf', head + b'data' + struct.pack('>q', size) + data[:size])
    s.add(f'cut_{size:04d}.caf', head + b'data' + struct.pack('>q', len(data)) + data[:size])
    s.add(f'unknown_{size:04d}.caf', head + b'data' + struct.pack('>q', -1) + data[:size])
    s.add(f'free_{size:04d}.caf', head + b'data' + struct.pack('>q', size) + data[:size] + b'free' + struct.pack('>q', 8) + b'\0' * 8)
s.close()

# caf_chunks
s = Set('caf_chunks')
extra = {b'kuki': body[b'kuki'], b'chan': body[b'chan'], b'info': body[b'info'], b'pakt': body[b'pakt'], b'free': b'\0' * 8, b'zzzz': b'\x01' * 8}
for mask in range(64):
    chosen = [(k, b) for i, (k, b) in enumerate(extra.items()) if mask >> i & 1]
    tag = '_'.join(k.decode() for k, _ in chosen) or 'none'
    base = [(b'desc', body[b'desc'])] + chosen
    s.add(f'{tag}_nodata.caf', join(base))
    s.add(f'{tag}_data0.caf', join(base) + b'data' + struct.pack('>q', 3013))
    s.add(f'{tag}_data4.caf', join(base) + b'data' + struct.pack('>q', 3013) + b'\0' * 4)
s.close()

# caf_desc: offsets in the body: flags 12, bytes per packet 16, frames per packet 20, channels 24, bits 28, format id 8
s = Set('caf_desc')
no_kuki = [(k, b) for k, b in parts if k != b'kuki']
no_pakt = [(k, b) for k, b in parts if k != b'pakt']
for fpp in (0, 1, 512, 1024, 2048, 4096, 65535, 65536, 999_999, 1_000_000, 1 << 24, 1 << 31, 0xffffffff):
    s.add(f'fpp{fpp}.caf', join(with_desc(parts, 20, fpp)))
for bpp, fpp in itertools.product((0, 1, 100), (0, 1024)):
    for tag, p in (('pakt', parts), ('nopakt', no_pakt), ('nokuki', no_kuki)):
        s.add(f'bpp{bpp}_fpp{fpp}_{tag}.caf', join(with_desc(with_desc(p, 16, bpp), 20, fpp)))
for ch in range(17):
    s.add(f'ch{ch}_kuki.caf', join(with_desc(parts, 24, ch)))
    s.add(f'ch{ch}_nokuki.caf', join(with_desc(no_kuki, 24, ch)))
for bits in (0, 1, 8, 16, 24, 32, 64):
    s.add(f'bits{bits}.caf', join(with_desc(parts, 28, bits)))
for flags in (0, 1, 2, 3, 4, 8, 0x80000000, 0xffffffff):
    s.add(f'flags{flags}.caf', join(with_desc(parts, 12, flags)))
for fid in (b'aac ', b'aach', b'aacp', b'aacl', b'aace', b'aacm', b'alac', b'lpcm', b'.mp1', b'.mp2', b'.mp3', b'ac-3', b'ima4', b'xxxx', b'\0\0\0\0'):
    name = fid.decode('latin1').strip().replace('.', '_').replace('\0', '0')
    for tag, p in (('kuki', parts), ('nokuki', no_kuki)):
        for ch in (1, 2):
            s.add(f'format_{name}_{tag}_ch{ch}.caf', join(with_desc(with_desc(p, 8, fid, '>4s'), 24, ch)))
for size in (0, 4, 8, 11, 12, 20):
    s.add(f'chan_size{size}.caf', caf.replace(b'chan' + struct.pack('>q', 12), b'chan' + struct.pack('>q', size), 1))
s.close()

# caf_rates
s = Set('caf_rates')
for rate in [0.5, 1, 2, 3, 7, 10, 11, 22, 22.9999, 23, 24, 25, 50, 100, 8000, 7349, 7350, 7351, 44099, 44100, 44100.5, 44101, 48000, 96000, 110851,
             110852, 150000, 192000, 1e6, 2999999, 2999999.5, 3e6, 3000001, 1e300, 1e-300, 0.0, -0.0, -1.0, float('nan'), float('inf'), float('-inf')]:
    s.add(f'rate_{rate!r}.caf'.replace('.', '_', 1).replace('-', 'm'), join([(k, struct.pack('>d', rate) + b[8:] if k == b'desc' else b) for k, b in parts]))
s.close()

# adts_header
s = Set('adts_header')
frames, pos = [], 0
while pos + 7 <= len(adts):
    length = ((adts[pos + 3] & 3) << 11) | (adts[pos + 4] << 3) | (adts[pos + 5] >> 5)
    frames.append((pos, length))
    pos += length
fields = [('id', 1, 3, 1), ('layer', 1, 1, 2), ('prot', 1, 0, 1), ('profile', 2, 6, 2), ('sfi', 2, 2, 4), ('priv', 2, 1, 1), ('cc_hi', 2, 0, 1),
          ('cc_lo', 3, 6, 2), ('orig', 3, 5, 1), ('home', 3, 4, 1), ('cid', 3, 3, 1), ('cstart', 3, 2, 1), ('fullhi', 5, 0, 5), ('nrdb', 6, 0, 2)]
for name, byte, shift, width in fields:
    for frame in (0, 1, 5):
        for value in range(1 << width):
            data = bytearray(adts)
            mask = ((1 << width) - 1) << shift
            at = frames[frame][0] + byte
            if (data[at] & mask) >> shift == value:
                continue
            data[at] = (data[at] & ~mask & 0xff) | (value << shift)
            s.add(f'{name}_f{frame}_v{value}.aac', bytes(data))
s.close()


def rebuilt(modify):
    """The ADTS file with `modify(index, header, access unit)` -> (header, access unit, crc present)."""
    out_bytes = b''
    for index, (at, length) in enumerate(frames):
        header, unit = bytearray(adts[at:at + 7]), adts[at + 7:at + length]
        header, unit, crc = modify(index, header, unit)
        total = 7 + (2 if crc else 0) + len(unit)
        header[1] = (header[1] & 0xfe) | (0 if crc else 1)
        header[3] = (header[3] & 0xfc) | (total >> 11)
        header[4] = (total >> 3) & 0xff
        header[5] = ((total & 7) << 5) | (header[5] & 0x1f)
        out_bytes += bytes(header) + (b'\0\0' if crc else b'') + unit
    return out_bytes


# adts_crc
s = Set('adts_crc')
s.add('all_plain.aac', rebuilt(lambda i, h, u: (h, u, False)))
s.add('all_crc.aac', rebuilt(lambda i, h, u: (h, u, True)))
for k in (1, 2, 5):
    s.add(f'plain_then_crc{k}.aac', rebuilt(lambda i, h, u, k=k: (h, u, i >= k)))
    s.add(f'plain_one_crc{k}.aac', rebuilt(lambda i, h, u, k=k: (h, u, i == k)))
    s.add(f'crc_then_plain{k}.aac', rebuilt(lambda i, h, u, k=k: (h, u, i < k)))
    s.add(f'crc_one_plain{k}.aac', rebuilt(lambda i, h, u, k=k: (h, u, i != k)))
s.add('first_crc.aac', rebuilt(lambda i, h, u: (h, u, i == 0)))
for b1, profile, sfi in itertools.product((0xf0, 0xf1, 0xf8, 0xf9), range(4), (3, 4)):
    def mark(i, h, u, b1=b1, profile=profile, sfi=sfi):
        if i == 0:
            h[1] = b1
            h[2] = (h[2] & 0x03) | (profile << 6) | (sfi << 2)
        return h, u, False
    s.add(f'first_b{b1:02x}_p{profile}_s{sfi}.aac', rebuilt(mark))
s.close()


# adts_sbr: a fill element in front of the first element of some access units
def bits(data):
    return ''.join(f'{b:08b}' for b in data)


def from_bits(text):
    text += '0' * (-len(text) % 8)
    return bytes(int(text[i:i + 8], 2) for i in range(0, len(text), 8))


def fill(payload, count):
    return '110' + f'{count:04b}' + (payload + '0' * 8 * count)[:8 * count]


s = Set('adts_sbr')
for ext in (0, 1, 2, 10, 11, 13, 14, 15):
    for where, which in (('all', lambda i: True), ('first', lambda i: i == 0), ('second', lambda i: i == 1), ('last', lambda i: i == len(frames) - 1)):
        s.add(f'ext{ext}_{where}.aac', rebuilt(lambda i, h, u, ext=ext, which=which: (h, from_bits(fill(f'{ext:04b}', 2) + bits(u)) if which(i) else u, False)))
for name, payload, count in (('h1_zero', '11011' + '0' * 20, 4), ('h1_ones', '11011' + '1' * 60, 8), ('h1_n1', '11011', 1), ('h14', '1110' + '0' * 10 + '1', 6)):
    s.add(f'sbr_{name}_first.aac', rebuilt(lambda i, h, u, p=payload, c=count: (h, from_bits(fill(p, c) + bits(u)) if i == 0 else u, False)))
s.close()

# loas_cut
s = Set('loas_cut')
units, pos = [], 0
while pos + 3 <= len(loas):
    units.append((pos, ((loas[pos + 1] & 0x1f) << 8) | loas[pos + 2]))
    pos += 3 + units[-1][1]
for length in (units[0][1] + n for n in (1, 2, 10, 100)):
    s.add(f'first_length_{length}.loas', loas[:1] + bytes([(loas[1] & 0xe0) | (length >> 8), length & 0xff]) + loas[3:])
for length in (len(loas) - 3, len(loas) - 2, len(loas), len(loas) + 100, 4112, 8191):
    s.add(f'first_length_{length}.loas', loas[:1] + bytes([(loas[1] & 0xe0) | (length >> 8), length & 0xff]) + loas[3:])
for n in range(3, 21):
    s.add(f'prefix_{n}.loas', loas[:n])
s.add('drop_first.loas', loas[units[1][0]:])
s.add('first_then_junk.loas', loas[:units[1][0]] + b'\xaa' * 20)
s.close()

# mp4_atoms: ftyp 0..28, moov 28..890, free 890..4088, mdat after it
s = Set('mp4_atoms')
for size in (0, 3, 8, 19, 20, 27, 28, 29, 0xffffffff):
    s.add(f'ftyp_{size}.m4a', struct.pack('>I', size) + m4a[4:])
for size in (3, 8, 861, 862, 863, 0xffff, 0xffffffff):
    s.add(f'moov_{size}.m4a', m4a[:28] + struct.pack('>I', size) + m4a[32:])
for size in (10000, 1 << 40, (1 << 63) - 1, 1 << 63, (1 << 64) - 1):
    # the 64-bit header costs 8 bytes: the free atom gives them back, the media data keeps its offset
    s.add(f'moov_wide_{size}.m4a', m4a[:28] + struct.pack('>I4sQ', 1, b'moov', size) + m4a[36:890] + struct.pack('>I', 3190) + m4a[894:898] + m4a[906:])
for n in (8, 28, 40, 100, 890):
    s.add(f'prefix_{n}.m4a', m4a[:n])
s.close()

# names
s = Set('names')
contents = {'adts': adts, 'm4a': m4a, 'caf': caf, 'loas': loas, 'garbage': bytes(range(256)) * 2, 'empty': b''}
for ext in ('ogg', 'oga', 'opus', 'sd2', 'spx', 'ac3', 'm4v', 'amr', 'au', 'snd', 'w64', 'gsm', 'mp4', 'caf', 'mov', 'aac', 'adts', 'loas', 'latm'):
    for name, content in contents.items():
        s.add(f'{name}.{ext}', content)
s.close()
