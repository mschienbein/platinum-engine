// Compile this file as Objective-C++ against the caller's pinned, open-source
// TagLib 1.11.1 build. It does not load or link the application's MIKTag/TagLib.
// Original-to-original byte parity and vendor patch identity remain unproved.
#include "taglib_bridge.h"
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <ImageIO/ImageIO.h>
#import <CoreServices/CoreServices.h>
#import <CommonCrypto/CommonDigest.h>

#include <aifffile.h>
#include <attachedpictureframe.h>
#include <commentsframe.h>
#include <flacfile.h>
#include <flacpicture.h>
#include <generalencapsulatedobjectframe.h>
#include <id3v1tag.h>
#include <id3v2framefactory.h>
#include <id3v2header.h>
#include <id3v2tag.h>
#include <infotag.h>
#include <mp4atom.h>
#include <mp4coverart.h>
#include <mp4file.h>
#include <mp4item.h>
#include <mp4tag.h>
#include <mpegfile.h>
#include <mpegheader.h>
#include <textidentificationframe.h>
#include <tfilestream.h>
#include <tpropertymap.h>
#include <wavfile.h>
#include <xiphcomment.h>
#include <climits>
#include <cstdio>
#include <cstring>
#include <memory>
#include <optional>
#include <stdexcept>
#include <string>
#include <vector>

using namespace TagLib;
namespace {
using ID3 = ID3v2::Tag;
using Frame = ID3v2::Frame;
using Picture = ID3v2::AttachedPictureFrame;
using Comment = ID3v2::CommentsFrame;
using GEOB = ID3v2::GeneralEncapsulatedObjectFrame;

class WavFile : public RIFF::WAV::File {
public:
    WavFile(IOStream *s) : RIFF::WAV::File(s, false) {}
    ByteVector firstList() {
        for (uint i = 0; i < chunkCount(); ++i)
            if (chunkName(i) == "LIST") return chunkData(i);
        return ByteVector::null;
    }
    void setFirstList(const ByteVector &data) { setChunkData("LIST", data); }
};

void require(bool condition, const char *message) {
    if (!condition) throw std::runtime_error(message);
}

String from_ns(NSString *s) {
    if (!s) return String::null;
    // NSString(TagLib)::TagLibString passes UTF8String directly, including
    // its C-string termination behavior, rather than a length-bearing NSData.
    const char *utf8 = [s UTF8String];
    require(utf8 != nullptr, "string cannot be represented in TagLib");
    return String(utf8, String::UTF8);
}
NSString *to_ns(const String &s) {
    if (s.isNull()) return nil;
    std::string utf8 = s.to8Bit(true);
    return [[NSString alloc] initWithBytes:utf8.data() length:utf8.size() encoding:NSUTF8StringEncoding];
}

Comment *find_comment(ID3 *tag) {
    if (!tag) return nullptr;
    Comment *result = nullptr;
    const auto &frames = tag->frameList("COMM");
    for (auto it = frames.begin(); it != frames.end(); ++it) {
        Comment *c = dynamic_cast<Comment *>(*it);
        if (!c || !c->description().isEmpty()) continue;
        ByteVector lang = c->language();
        if (lang != "XXX" && lang != "xxx" && lang != "und" && lang != "Pur" && lang != "lZ2") result = c;
    }
    return result;
}
void set_comment(ID3 *tag, const String &value) {
    if (!tag) return;
    Comment *chosen = find_comment(tag);
    if (!value.isEmpty()) {
        if (!chosen) {
            chosen = new Comment(ID3v2::FrameFactory::instance()->defaultTextEncoding());
            chosen->setLanguage("eng");
            tag->addFrame(chosen);
        }
        chosen->setText(value);
    } else if (chosen) {
        tag->removeFrame(chosen, true);
        // After deletion the reference loop visits every remaining COMM.
        chosen = nullptr;
    } else return;
    std::vector<Frame *> previous;
    const auto &comments = tag->frameList("COMM");
    for (auto i = comments.begin(); i != comments.end() && *i != chosen; ++i) previous.push_back(*i);
    for (Frame *f : previous) { tag->removeFrame(f, false); tag->addFrame(f); }
}

Frame *first_frame(ID3 *tag, const char *id) {
    if (!tag) return nullptr;
    const auto &frames = tag->frameList(id);
    return frames.isEmpty() ? nullptr : frames.front();
}
void set_text(ID3 *tag, const char *id, const String &s) {
    if (!tag) return;
    Frame *f = first_frame(tag, id);
    if (!f) {
        f = new ID3v2::TextIdentificationFrame(id, ID3v2::FrameFactory::instance()->defaultTextEncoding());
        tag->addFrame(f);
    }
    f->setText(s);
}
String frame_text(ID3 *tag, const char *id) {
    Frame *f = first_frame(tag, id);
    return f ? f->toString() : String::null;
}

struct State {
    int format;
    std::unique_ptr<FileStream> stream;
    std::unique_ptr<TagLib::File> file;
    MPEG::File *mp3 = nullptr;
    MP4::File *mp4 = nullptr;
    WavFile *wav = nullptr;
    RIFF::AIFF::File *aiff = nullptr;
    FLAC::File *flac = nullptr;
    ID3 *v2 = nullptr;
    ID3v1::Tag *v1 = nullptr;
    Ogg::XiphComment *xiph = nullptr;
    MP4::Tag *m4 = nullptr;

    State(int kind, const char *path, bool write, bool normalize_id3 = true) : format(kind), stream(new FileStream(path, !write)) {
        require(stream->isOpen(), "cannot open tag file");
        switch (kind) {
        case PN_TAG_MP3:
            mp3 = new MPEG::File(stream.get(), ID3v2::FrameFactory::instance(), false);
            file.reset(mp3);
            break;
        case PN_TAG_MP4:
            mp4 = new MP4::File(stream.get(), false); file.reset(mp4); break;
        case PN_TAG_WAV:
            wav = new WavFile(stream.get()); file.reset(wav); break;
        case PN_TAG_AIFF:
            aiff = new RIFF::AIFF::File(stream.get(), false); file.reset(aiff); break;
        case PN_TAG_FLAC:
            flac = new FLAC::File(stream.get(), ID3v2::FrameFactory::instance(), false);
            file.reset(flac); break;
        default: throw std::runtime_error("unknown tag format");
        }
        require(file->isValid(), "invalid source or destination container");
        switch (kind) {
        case PN_TAG_MP3:
            v1 = mp3->ID3v1Tag(false);
            if (v1 && v1->isEmpty()) v1 = nullptr;
            v2 = mp3->ID3v2Tag(true);
            if (normalize_id3 && v2 && first_frame(v2, "TCON")) v2->setGenre(v2->genre());
            break;
        case PN_TAG_MP4: m4 = mp4->tag(); break;
        case PN_TAG_WAV: v2 = wav->tag(); break;
        case PN_TAG_AIFF: v2 = aiff->tag(); break;
        case PN_TAG_FLAC:
            xiph = flac->xiphComment(true); v2 = flac->ID3v2Tag(false); v1 = flac->ID3v1Tag(false); break;
        }
        require((kind != PN_TAG_MP4 || m4) && (kind != PN_TAG_WAV || v2) &&
                (kind != PN_TAG_AIFF || v2) && (kind != PN_TAG_FLAC || xiph), "tag object unavailable");
    }
    Tag *primary() const { return format == PN_TAG_MP4 ? static_cast<Tag *>(m4) : static_cast<Tag *>(v2); }
    String id3_string(int which) const {
        String value = String::null;
        if (v2) {
            if (which == 0) value = v2->artist();
            else if (which == 1) value = v2->album();
            else if (which == 2) value = v2->title();
            else { Comment *f = find_comment(v2); if (f) value = f->toString(); }
        }
        if (value.isNull() && v1) {
            if (which == 0) value = v1->artist();
            else if (which == 1) value = v1->album();
            else if (which == 2) value = v1->title();
            else value = v1->comment();
        }
        return value;
    }
    static String get_string(Tag *tag, int which) {
        if (!tag) return String::null;
        if (which == 0) return tag->artist();
        if (which == 1) return tag->album();
        if (which == 2) return tag->title();
        return tag->comment();
    }
    static void put_string(Tag *tag, int which, const String &s) {
        if (!tag) return;
        if (which == 0) tag->setArtist(s);
        else if (which == 1) tag->setAlbum(s);
        else if (which == 2) tag->setTitle(s);
        else tag->setComment(s);
    }
    String get_string(int which) const {
        if (m4) return get_string(m4, which);
        if (flac) {
            String s = get_string(xiph, which);
            if (which == 3 && s.isEmpty()) {
                auto fields = xiph->fieldListMap();
                if (fields.contains("COMMENT")) s = fields["COMMENT"].toString(" ");
            }
            return s.isEmpty() ? id3_string(which) : s;
        }
        String s = id3_string(which);
        if (wav && s.isEmpty()) return get_string(wav->InfoTag(), which);
        return s;
    }
    void put_string(int which, const String &s) {
        if (m4) { put_string(m4, which, s); return; }
        if (xiph) {
            put_string(xiph, which, s);
            if (which == 3) xiph->addField("COMMENT", s, true);
        }
        if (which == 3) set_comment(v2, s); else put_string(v2, which, s);
        put_string(v1, which, s);
        if (wav) put_string(wav->InfoTag(), which, s);
    }
    long long year() const {
        if (m4) return m4->year();
        if (xiph) return xiph->year();
        unsigned n = v2 ? v2->year() : 0;
        if (!n && v1) n = v1->year();
        if (!n && wav) n = wav->InfoTag()->year();
        return n;
    }
    void set_year(long long n) {
        unsigned year = n < 1 ? 0 : static_cast<unsigned>(n);
        if (m4) { m4->setYear(year); return; }
        if (xiph) { xiph->setYear(year); return; }
        if (v2) v2->setYear(year);
        if (v1) v1->setYear(year);
        if (wav) wav->InfoTag()->setYear(static_cast<unsigned>(n));
    }
    long long tempo() const {
        if (m4) { auto &items = m4->itemListMap(); return items.contains("tmpo") ? items["tmpo"].toInt() : 0; }
        if (xiph) {
            auto fields = xiph->fieldListMap();
            if (fields.contains("BPM")) { int n = fields["BPM"].toString(" ").toInt(); if (n > 0) return n; }
        }
        return frame_text(v2, "TBPM").toInt();
    }
    void set_tempo(long long n) {
        if (m4) { m4->itemListMap().insert("tmpo", MP4::Item(static_cast<int>(n))); return; }
        if (xiph) { if (n < 1) xiph->removeField("BPM"); else xiph->addField("BPM", String::number(static_cast<int>(n)), true); }
        set_text(v2, "TBPM", String::number(static_cast<int>(n)));
    }
    String key() const {
        if (m4) {
            auto &items = m4->itemListMap();
            return items.contains("----:com.apple.iTunes:initialkey") ? items["----:com.apple.iTunes:initialkey"].toStringList().toString(" ") : String::null;
        }
        if (xiph) {
            auto fields = xiph->fieldListMap();
            if (fields.contains("INITIALKEY")) { String s = fields["INITIALKEY"].toString(" "); if (!s.isEmpty()) return s; }
        }
        return frame_text(v2, "TKEY");
    }
    void set_key(const String &s) {
        if (m4) {
            auto &items = m4->itemListMap();
            if (s.isNull()) items.erase("----:com.apple.iTunes:initialkey");
            else items.insert("----:com.apple.iTunes:initialkey", MP4::Item(StringList(s)));
            return;
        }
        if (xiph) { if (s.isEmpty()) xiph->removeField("INITIALKEY"); else xiph->addField("INITIALKEY", s, true); }
        set_text(v2, "TKEY", s.isNull() ? String("") : s);
    }
    bool supports_images() const { return format != PN_TAG_WAV; }
    bool has_images() const {
        if (wav) return false;
        if (m4) { auto &m = m4->itemListMap(); return m.contains("covr") && !m["covr"].toCoverArtList().isEmpty(); }
        if (flac) return !flac->pictureList().isEmpty();
        return first_frame(v2, "APIC") != nullptr;
    }
    NSArray<NSImage *> *images() const {
        NSMutableArray<NSImage *> *result = [NSMutableArray array];
        auto append = [&](const ByteVector &b) {
            NSData *d = [NSData dataWithBytes:b.data() length:b.size()];
            NSImage *im = [[NSImage alloc] initWithData:d];
            if (im) [result addObject:im];
        };
        if (m4) {
            auto &items = m4->itemListMap();
            if (items.contains("covr")) { auto list = items["covr"].toCoverArtList(); for (auto i = list.begin(); i != list.end(); ++i) append(i->data()); }
        } else if (flac) {
            auto list = flac->pictureList(); for (auto i = list.begin(); i != list.end(); ++i) append((*i)->data());
        } else if (v2 && !wav) {
            const auto &frames = v2->frameList("APIC");
            for (auto i = frames.begin(); i != frames.end(); ++i) { Picture *p = dynamic_cast<Picture *>(*i); if (p) append(p->picture()); }
        }
        return result;
    }
    void set_images(NSArray<NSImage *> *images) {
        if (!supports_images()) return;
        if (flac) flac->removePictures();
        else if (v2) v2->removeFrames("APIC");
        MP4::CoverArtList covers;
        for (NSImage *image in images) {
            CFMutableDataRef data = CFDataCreateMutable(nullptr, 0);
            require(data != nullptr, "cannot allocate PNG buffer");
            CGImageDestinationRef destination = CGImageDestinationCreateWithData(data, kUTTypePNG, 1, nullptr);
            if (!destination) { CFRelease(data); throw std::runtime_error("cannot create PNG destination"); }
            CGImageRef cg = [image CGImageForProposedRect:nullptr context:nil hints:nil];
            if (cg) CGImageDestinationAddImage(destination, cg, nullptr);
            bool good = cg && CGImageDestinationFinalize(destination);
            if (!good) { CFRelease(destination); CFRelease(data); throw std::runtime_error("cannot encode artwork as PNG"); }
            {
                CFIndex size = CFDataGetLength(data);
                if (size < 0 || static_cast<unsigned long>(size) > UINT_MAX) {
                    CFRelease(destination); CFRelease(data); throw std::runtime_error("PNG exceeds TagLib limit");
                }
                ByteVector bytes(reinterpret_cast<const char *>(CFDataGetBytePtr(data)), static_cast<uint>(size));
                if (m4) covers.append(MP4::CoverArt(MP4::CoverArt::PNG, bytes));
                else if (flac) { auto *p = new FLAC::Picture; p->setMimeType("image/png"); p->setData(bytes); flac->addPicture(p); }
                else if (v2) { auto *p = new Picture; p->setPicture(bytes); p->setMimeType("image/png"); v2->addFrame(p); }
            }
            CFRelease(destination); CFRelease(data);
        }
        if (m4) m4->itemListMap().insert("covr", MP4::Item(covers));
    }
    bool save() {
        if (mp3) {
            // MIKTagID3 saves existing ID3v1 first, then ID3v2. Its v1-comment
            // workaround prevents TagLib from duplicating a different v2 comment.
            if (v1) {
                bool restore = false;
                if (v1->comment().isEmpty() && v2 && v2->comment() != id3_string(3)) { v1->setComment(" "); restore = true; }
                bool ok = mp3->save(MPEG::File::ID3v1, false);
                if (restore) v1->setComment("");
                if (!ok) return false;
            }
            return v2 && mp3->save(MPEG::File::ID3v2, false);
        }
        return file->save();
    }
};

pn_tag_payload marker_data(pn_tag_serialize_marker serialize, void *context) {
    pn_tag_payload value{};
    require(serialize && serialize(context, &value) == 0, "marker serialization failed");
    require(!value.present || (value.length <= UINT_MAX && (value.bytes || value.length == 0)), "invalid marker payload");
    return value;
}
// The ABI permits a null pointer for a present empty payload. TagLib 1.11.1's
// ByteVector range constructor performs pointer arithmetic even for length 0.
ByteVector payload_bytes(const pn_tag_payload &value) {
    const char *bytes = value.length ? reinterpret_cast<const char *>(value.bytes) : "";
    return ByteVector(bytes, static_cast<uint>(value.length));
}
void set_geob(ID3 *tag, pn_tag_serialize_marker serialize, void *context) {
    if (!tag) return;
    GEOB *found = nullptr;
    const auto &frames = tag->frameList("GEOB");
    for (auto it = frames.begin(); it != frames.end(); ++it) {
        GEOB *g = dynamic_cast<GEOB *>(*it);
        if (g && g->description() == "PlatinumNotes") { found = g; break; }
    }
    pn_tag_payload value = marker_data(serialize, context);
    if (!value.present) { if (found) tag->removeFrame(found); return; }
    require(value.length <= UINT_MAX && (value.bytes || value.length == 0), "invalid marker payload");
    if (!found) { found = new GEOB; found->setDescription("PlatinumNotes"); found->setMimeType("application/json"); tag->addFrame(found); }
    found->setObject(payload_bytes(value));
}
void set_marker(State &s, pn_tag_serialize_marker serialize, void *context) {
    if (s.wav) return; // PLTN is serialized and written after TagLib's save.
    if (s.m4) {
        pn_tag_payload value = marker_data(serialize, context);
        auto &items = s.m4->itemListMap(); const String key("----:com.mixedinkey.platinumnotes:data");
        if (!value.present) items.erase(key);
        else {
            require(value.length <= UINT_MAX && (value.bytes || value.length == 0), "invalid marker payload");
            String text(payload_bytes(value), String::Latin1);
            items.insert(key, MP4::Item(StringList(text)));
        }
        return;
    }
    if (s.xiph) {
        pn_tag_payload value = marker_data(serialize, context);
        if (!value.present) s.xiph->removeField("PLATINUMNOTES");
        else {
            require(value.length <= UINT_MAX && (value.bytes || value.length == 0), "invalid marker payload");
            String text(payload_bytes(value), String::Latin1);
            s.xiph->addField("PLATINUMNOTES", text, true);
        }
        set_geob(s.v2, serialize, context);
    } else set_geob(s.v2, serialize, context);
}

// The parsed TagLib Xiph map discards empty values, making a present empty
// PLATINUMNOTES indistinguishable from absence. Inspect the first Vorbis block
// itself, preserving its malformed/empty payload for the caller to reject.
std::optional<std::vector<uint8_t>> flac_marker(State &s) {
    long start = 0;
    if (s.v2) {
        require(s.v2->header() != nullptr, "FLAC ID3 header unavailable");
        start = static_cast<long>(s.v2->header()->completeTagSize());
    }
    const long magic = s.flac->find(ByteVector("fLaC"), start);
    const long length = s.flac->length();
    require(magic >= 0 && length >= 0 && magic <= length - 4, "invalid FLAC marker location");
    long position = magic + 4;
    bool last = false;
    while (!last) {
        require(position <= length - 4, "truncated FLAC metadata header");
        s.flac->seek(position);
        ByteVector header = s.flac->readBlock(4);
        require(header.size() == 4, "cannot read FLAC metadata header");
        const auto *h = reinterpret_cast<const unsigned char *>(header.data());
        last = (h[0] & 0x80) != 0;
        const unsigned kind = h[0] & 0x7f;
        const uint32_t size = (uint32_t(h[1]) << 16) | (uint32_t(h[2]) << 8) | uint32_t(h[3]);
        position += 4;
        require(static_cast<long>(size) <= length - position, "truncated FLAC metadata block");
        if (kind == 4) {
            require(size != 0, "empty FLAC Vorbis comment block");
            s.flac->seek(position);
            ByteVector raw = s.flac->readBlock(size);
            require(raw.size() == size, "cannot read FLAC Vorbis comment block");
            const auto *p = reinterpret_cast<const uint8_t *>(raw.data());
            size_t cursor = 0;
            auto number = [&]() -> uint32_t {
                require(cursor <= size && size - cursor >= 4, "truncated FLAC Vorbis comment length");
                uint32_t n = uint32_t(p[cursor]) | (uint32_t(p[cursor + 1]) << 8) |
                             (uint32_t(p[cursor + 2]) << 16) | (uint32_t(p[cursor + 3]) << 24);
                cursor += 4; return n;
            };
            const uint32_t vendor = number();
            require(vendor <= size - cursor, "truncated FLAC Vorbis vendor");
            cursor += vendor;
            const uint32_t count = number();
            bool seen = false, have_nonempty = false;
            std::vector<uint8_t> value;
            static constexpr char key[] = "PLATINUMNOTES=";
            for (uint32_t i = 0; i < count; ++i) {
                const uint32_t n = number();
                require(n <= size - cursor, "truncated FLAC Vorbis field");
                bool matches = n >= sizeof(key) - 1;
                for (size_t j = 0; matches && j < sizeof(key) - 1; ++j) {
                    uint8_t c = p[cursor + j];
                    if (c >= 'a' && c <= 'z') c -= 'a' - 'A';
                    matches = c == static_cast<uint8_t>(key[j]);
                }
                if (matches) {
                    seen = true;
                    const size_t start_value = cursor + sizeof(key) - 1;
                    const size_t value_size = n - (sizeof(key) - 1);
                    if (value_size) {
                        if (have_nonempty) value.push_back(' ');
                        value.insert(value.end(), p + start_value, p + start_value + value_size);
                        have_nonempty = true;
                    }
                }
                cursor += n;
            }
            if (seen) return value;
            return std::nullopt; // TagLib uses the first Vorbis comment block.
        }
        position += size;
    }
    return std::nullopt;
}

std::optional<ByteVector> wav_marker(State &s, bool collect = true) {
    const long length = s.wav->length();
    require(length >= 12, "truncated WAV header");
    s.wav->seek(0);
    ByteVector header = s.wav->readBlock(12);
    require(header.size() == 12 && header.mid(0, 4) == "RIFF" && header.mid(8, 4) == "WAVE", "invalid WAV header");
    const auto *p = reinterpret_cast<const uint8_t *>(header.data());
    const uint32_t riff = uint32_t(p[4]) | (uint32_t(p[5]) << 8) | (uint32_t(p[6]) << 16) | (uint32_t(p[7]) << 24);
    const uint64_t end = uint64_t(riff) + 8;
    require(riff >= 4 && end <= static_cast<uint64_t>(length), "invalid WAV RIFF length");
    uint64_t position = 12;
    bool substantial_seen = false;
    std::optional<ByteVector> result;
    while (position < end) {
        require(end - position >= 8, "truncated WAV chunk header");
        s.wav->seek(static_cast<long>(position));
        ByteVector chunk = s.wav->readBlock(8);
        require(chunk.size() == 8, "cannot read WAV chunk header");
        const auto *q = reinterpret_cast<const uint8_t *>(chunk.data());
        // The original WAV parser accepts only ASCII letters, digits and
        // space in a chunk identifier. Treat its early stop as an inspection
        // error, not proof that a later marker is absent.
        for (int i = 0; i < 4; ++i)
            require((q[i] >= 'A' && q[i] <= 'Z') || (q[i] >= 'a' && q[i] <= 'z') ||
                    (q[i] >= '0' && q[i] <= '9') || q[i] == ' ', "invalid WAV chunk identifier");
        const uint32_t n = uint32_t(q[4]) | (uint32_t(q[5]) << 8) | (uint32_t(q[6]) << 16) | (uint32_t(q[7]) << 24);
        position += 8;
        require(uint64_t(n) + (n & 1) <= end - position, "truncated WAV chunk");
        if (collect && chunk.mid(0, 4) == "PLTN" && (n >= 4 || !substantial_seen)) {
            ByteVector bytes = s.wav->readBlock(n);
            require(bytes.size() == n, "cannot read WAV PLTN payload");
            result = bytes;
            if (n >= 4) substantial_seen = true;
        }
        position += uint64_t(n) + (n & 1);
    }
    return result;
}

std::optional<ByteVector> id3_marker(ID3 *tag) {
    if (!tag) return std::nullopt;
    const auto &frames = tag->frameList("GEOB");
    for (auto i = frames.begin(); i != frames.end(); ++i) {
        auto *g = dynamic_cast<GEOB *>(*i);
        if (g && g->description() == "PlatinumNotes") return g->object();
    }
    return std::nullopt;
}

void validate_source(State &source) {
    if (source.mp3) {
        // MPEG::File::isValid() checks its stream, not the presence of audio.
        // Without this, an arbitrary readable file would be reported as
        // successfully inspected and marker-free.
        long offset = source.mp3->firstFrameOffset();
        bool found = false;
        while (offset >= 0) {
            MPEG::Header header(source.mp3, offset, false);
            if (header.isValid() && header.frameLength() > 0 &&
                offset <= source.mp3->length() - header.frameLength()) { found = true; break; }
            offset = source.mp3->nextFrameOffset(offset + 1);
        }
        require(found, "invalid or truncated MPEG audio");
    }
    if (source.aiff) {
        require(source.aiff->length() >= 12, "truncated AIFF header");
        source.aiff->seek(0);
        ByteVector header = source.aiff->readBlock(12);
        require(header.size() == 12 && header.mid(0, 4) == "FORM" &&
                (header.mid(8, 4) == "AIFF" || header.mid(8, 4) == "AIFC"),
                "invalid AIFF header");
        const auto *p = reinterpret_cast<const uint8_t *>(header.data());
        const uint32_t size = (uint32_t(p[4]) << 24) | (uint32_t(p[5]) << 16) |
                              (uint32_t(p[6]) << 8) | uint32_t(p[7]);
        require(size >= 4 && uint64_t(size) + 8 <= static_cast<uint64_t>(source.aiff->length()),
                "invalid AIFF FORM length");
    }
}

void read_marker(int format, const char *path, pn_tag_receive_marker receive, void *context) {
    require(path && receive, "null marker path or receiver");
    State source(format, path, false);
    validate_source(source);
    if (source.flac) {
        auto xiph = flac_marker(source);
        if (xiph) {
            require(receive(context, xiph->data(), xiph->size(), 1) == 0, "marker receiver failed");
            return;
        }
    }
    std::optional<ByteVector> value;
    if (source.wav) value = wav_marker(source);
    else if (source.m4) {
        auto &items = source.m4->itemListMap();
        const String key("----:com.mixedinkey.platinumnotes:data");
        if (items.contains(key)) {
            const MP4::Item item = items[key];
            require(item.atomDataType() == MP4::TypeUTF8, "unsupported MP4 marker atom data type");
            value = item.toStringList().toString(" ").data(String::UTF8);
        }
    } else value = id3_marker(source.v2);
    const auto *bytes = value && !value->isEmpty() ? reinterpret_cast<const uint8_t *>(value->data()) : nullptr;
    require(receive(context, bytes, value ? value->size() : 0, value.has_value() ? 1 : 0) == 0, "marker receiver failed");
}

// These strings are projections of TagLib values, not inferred raw bytes.
id inspection_string(const String &value) {
    if (value.isNull()) return [NSNull null];
    NSString *s = to_ns(value);
    require(s != nil, "cannot encode TagLib string as UTF-8");
    return s;
}
NSString *required_inspection_string(const String &value) {
    id result = inspection_string(value);
    require(result != [NSNull null], "null TagLib property name or value");
    return (NSString *)result;
}

NSString *sha256_bytes(const ByteVector &bytes) {
    unsigned char digest[CC_SHA256_DIGEST_LENGTH];
    const char *data = bytes.isEmpty() ? "" : bytes.data();
    require(CC_SHA256(data, bytes.size(), digest) != nullptr, "cannot hash artwork");
    char text[CC_SHA256_DIGEST_LENGTH * 2 + 1];
    static const char hex[] = "0123456789abcdef";
    for (size_t i = 0; i < sizeof digest; ++i) {
        text[2 * i] = hex[digest[i] >> 4];
        text[2 * i + 1] = hex[digest[i] & 15];
    }
    text[sizeof text - 1] = '\0';
    return [NSString stringWithUTF8String:text];
}
NSDictionary *artwork_entry(NSString *container, id mime, id picture_type,
                            id description, id format_code, const ByteVector &bytes) {
    return @{@"container": container, @"mime_type": mime, @"picture_type": picture_type,
             @"description": description, @"format_code": format_code,
             @"byte_count": @(bytes.size()), @"sha256": sha256_bytes(bytes)};
}
bool has_disk_id3(const State &s) {
    if (s.mp3) return s.mp3->hasID3v2Tag();
    if (s.wav) return s.wav->hasID3v2Tag();
    if (s.aiff) return s.aiff->hasID3v2Tag();
    if (s.flac) return s.flac->hasID3v2Tag();
    return false;
}
PropertyMap source_properties(const State &s) {
    if (s.mp3) return s.mp3->properties();
    if (s.mp4) return s.mp4->properties();
    if (s.wav) return s.wav->properties();
    if (s.aiff) return s.aiff->properties();
    return s.flac->properties();
}

bool xiph_key(const uint8_t *entry, size_t size, const char *key) {
    const size_t length = std::strlen(key);
    if (size < length || entry[length - 1] != '=') return false;
    for (size_t i = 0; i < length; ++i) {
        uint8_t c = entry[i];
        if (c >= 'a' && c <= 'z') c -= 'a' - 'A';
        if (c != static_cast<uint8_t>(key[i])) return false;
    }
    return true;
}
void append_xiph_picture(NSMutableArray *out, const uint8_t *encoded, size_t size, bool flac_picture) {
    NSData *input = [NSData dataWithBytes:encoded length:size];
    NSData *decoded = [[NSData alloc] initWithBase64EncodedData:input options:0];
    require(decoded != nil && decoded.length <= UINT_MAX, "invalid Xiph artwork Base64");
    const char *data = decoded.length ? static_cast<const char *>(decoded.bytes) : "";
    ByteVector bytes(data, static_cast<uint>(decoded.length));
    if (!flac_picture) {
        [out addObject:artwork_entry(@"flac_xiph_coverart", [NSNull null], [NSNull null],
                                      [NSNull null], [NSNull null], bytes)];
        return;
    }
    // Validate sizes before TagLib's Picture::parse, whose unsigned additions
    // otherwise cannot distinguish a wrapped declared length.
    const auto *p = reinterpret_cast<const uint8_t *>(data);
    size_t cursor = 0;
    auto number = [&]() -> uint32_t {
        require(cursor <= decoded.length && decoded.length - cursor >= 4, "truncated Xiph picture");
        uint32_t n = (uint32_t(p[cursor]) << 24) | (uint32_t(p[cursor + 1]) << 16) |
                     (uint32_t(p[cursor + 2]) << 8) | uint32_t(p[cursor + 3]);
        cursor += 4; return n;
    };
    (void)number(); // picture type
    uint32_t mime_size = number();
    require(mime_size <= decoded.length - cursor, "truncated Xiph picture MIME");
    cursor += mime_size;
    uint32_t description_size = number();
    require(description_size <= decoded.length - cursor, "truncated Xiph picture description");
    cursor += description_size;
    for (int i = 0; i < 4; ++i) (void)number(); // width, height, depth, colours
    uint32_t image_size = number();
    require(image_size <= decoded.length - cursor, "truncated Xiph picture bytes");
    FLAC::Picture picture;
    require(picture.parse(bytes), "invalid Xiph picture block");
    [out addObject:artwork_entry(@"flac_xiph_picture", inspection_string(picture.mimeType()),
        @(static_cast<uint32_t>(picture.type())), inspection_string(picture.description()),
        [NSNull null], picture.data())];
}
NSUInteger append_xiph_artwork_and_count_native_pictures(State &s, NSMutableArray *out) {
    require(!s.v2 || s.v2->header(), "FLAC ID3 header unavailable");
    const long start = s.v2 ? static_cast<long>(s.v2->header()->completeTagSize()) : 0;
    const long length = s.flac->length();
    const long magic = s.flac->find(ByteVector("fLaC"), start);
    require(length >= 4 && magic >= 0 && magic <= length - 4, "invalid FLAC metadata location");
    long position = magic + 4;
    bool last = false, comment_seen = false;
    NSUInteger pictures = 0;
    while (!last) {
        require(position <= length - 4, "truncated FLAC metadata header");
        s.flac->seek(position);
        ByteVector header = s.flac->readBlock(4);
        require(header.size() == 4, "cannot read FLAC metadata header");
        const auto *h = reinterpret_cast<const uint8_t *>(header.data());
        last = (h[0] & 0x80) != 0;
        const unsigned kind = h[0] & 0x7f;
        if (kind == 6) ++pictures;
        const uint32_t size = (uint32_t(h[1]) << 16) | (uint32_t(h[2]) << 8) | uint32_t(h[3]);
        position += 4;
        require(static_cast<long>(size) <= length - position, "truncated FLAC metadata block");
        if (kind == 4 && !comment_seen) {
            comment_seen = true; // TagLib uses the first Vorbis comment block.
            ByteVector raw = s.flac->readBlock(size);
            require(raw.size() == size, "truncated FLAC Vorbis comment");
            const auto *p = reinterpret_cast<const uint8_t *>(raw.data());
            size_t cursor = 0;
            auto number = [&]() -> uint32_t {
                require(cursor <= size && size - cursor >= 4, "truncated FLAC Vorbis comment");
                uint32_t n = uint32_t(p[cursor]) | (uint32_t(p[cursor + 1]) << 8) |
                             (uint32_t(p[cursor + 2]) << 16) | (uint32_t(p[cursor + 3]) << 24);
                cursor += 4; return n;
            };
            const uint32_t vendor_size = number();
            require(vendor_size <= size - cursor, "truncated FLAC Vorbis vendor");
            cursor += vendor_size;
            const uint32_t count = number();
            for (uint32_t i = 0; i < count; ++i) {
                const uint32_t field_size = number();
                require(field_size <= size - cursor, "truncated FLAC Vorbis field");
                const uint8_t *field = p + cursor;
                if (xiph_key(field, field_size, "METADATA_BLOCK_PICTURE="))
                    append_xiph_picture(out, field + 23, field_size - 23, true);
                else if (xiph_key(field, field_size, "COVERART="))
                    append_xiph_picture(out, field + 9, field_size - 9, false);
                cursor += field_size;
            }
            // Continue scanning to identify malformed native picture blocks.
        }
        position += size;
    }
    return pictures;
}

// TagLib 1.11.1's parseCovr silently discards unknown data types. Inspect the
// selected ilst's covr data children directly, preserving their format codes
// and byte payloads instead of presenting that omission as an empty inventory.
void append_mp4_artwork(State &s, NSMutableArray *out) {
    MP4::Atoms atoms(s.mp4);
    MP4::Atom *ilst = atoms.find("moov", "udta", "meta", "ilst");
    if (!ilst) return;
    const long file_length = s.mp4->length();
    for (auto i = ilst->children.begin(); i != ilst->children.end(); ++i) {
        const MP4::Atom *cover = *i;
        if (cover->name != "covr") continue;
        require(cover->offset >= 0 && cover->length >= 8 &&
                cover->offset <= file_length - cover->length, "invalid MP4 cover atom");
        s.mp4->seek(cover->offset);
        ByteVector outer = s.mp4->readBlock(8);
        require(outer.size() == 8, "truncated MP4 cover header");
        long pos = cover->offset + (outer.toUInt() == 1 ? 16 : 8);
        const long end = cover->offset + cover->length;
        require(pos <= end, "invalid MP4 cover header size");
        while (pos < end) {
            require(end - pos >= 16, "truncated MP4 cover data");
            s.mp4->seek(pos);
            ByteVector header = s.mp4->readBlock(8);
            require(header.size() == 8 && header.mid(4, 4) == "data", "invalid MP4 cover data atom");
            uint64_t length = header.toUInt();
            long header_size = 8;
            if (length == 1) {
                ByteVector extended = s.mp4->readBlock(8);
                require(extended.size() == 8, "truncated MP4 extended cover length");
                length = extended.toLongLong();
                header_size = 16;
            } else if (length == 0) length = static_cast<uint64_t>(end - pos);
            require(length >= static_cast<uint64_t>(header_size + 8) &&
                    length <= static_cast<uint64_t>(end - pos), "invalid MP4 cover data length");
            ByteVector type_locale = s.mp4->readBlock(8);
            require(type_locale.size() == 8, "truncated MP4 cover type");
            const uint32_t type = type_locale.toUInt();
            const uint64_t size = length - header_size - 8;
            require(size <= UINT_MAX, "MP4 artwork exceeds TagLib byte limit");
            ByteVector bytes = s.mp4->readBlock(static_cast<unsigned long>(size));
            require(bytes.size() == size, "truncated MP4 artwork");
            id mime = [NSNull null];
            if (type == MP4::TypeJPEG) mime = @"image/jpeg";
            else if (type == MP4::TypePNG) mime = @"image/png";
            else if (type == MP4::TypeGIF) mime = @"image/gif";
            else if (type == MP4::TypeBMP) mime = @"image/bmp";
            [out addObject:artwork_entry(@"mp4_covr", mime, [NSNull null], [NSNull null], @(type), bytes)];
            pos += static_cast<long>(length);
        }
    }
}

void inspect_source(int format, const char *path, pn_tag_receive_inspection receive, void *context) {
    require(path && receive, "null inspection path or receiver");
    // Do not normalize TCON: property and frame inventory describe the source
    // as parsed, not the in-memory genre rewrite used by transfer.
    State source(format, path, false, false);
    validate_source(source);
    if (source.wav) (void)wav_marker(source, false);

    PropertyMap map = source_properties(source);
    NSMutableDictionary *properties = [NSMutableDictionary dictionary];
    for (auto i = map.begin(); i != map.end(); ++i) {
        NSString *key = required_inspection_string(i->first);
        NSMutableArray *values = [NSMutableArray array];
        for (auto v = i->second.begin(); v != i->second.end(); ++v)
            [values addObject:required_inspection_string(*v)];
        properties[key] = values;
    }
    NSMutableArray *unsupported = [NSMutableArray array];
    const StringList &unmapped = map.unsupportedData();
    for (auto i = unmapped.begin(); i != unmapped.end(); ++i)
        [unsupported addObject:required_inspection_string(*i)];

    id id3_frames = [NSNull null];
    NSMutableArray *artwork = [NSMutableArray array];
    if (has_disk_id3(source)) {
        require(source.v2 != nullptr, "ID3v2 tag unavailable");
        NSMutableArray *frames = [NSMutableArray array];
        const auto &list = source.v2->frameList();
        for (auto i = list.begin(); i != list.end(); ++i) {
            ByteVector id_bytes = (*i)->frameID();
            NSString *frame_id = [[NSString alloc] initWithBytes:id_bytes.data() length:id_bytes.size() encoding:NSASCIIStringEncoding];
            require(frame_id != nil, "ID3 frame identifier is not ASCII");
            [frames addObject:@{@"id": frame_id, @"byte_count": @((*i)->size())}];
            if (id_bytes == "APIC") {
                auto *picture = dynamic_cast<Picture *>(*i);
                require(picture != nullptr, "unrecognized ID3 attached picture");
                [artwork addObject:artwork_entry(@"id3v2", inspection_string(picture->mimeType()),
                    @(static_cast<uint32_t>(picture->type())), inspection_string(picture->description()),
                    [NSNull null], picture->picture())];
            }
        }
        id3_frames = frames;
    }
    if (source.flac) {
        auto pictures = source.flac->pictureList();
        require(append_xiph_artwork_and_count_native_pictures(source, artwork) == pictures.size(),
                "FLAC picture block could not be parsed");
        for (auto i = pictures.begin(); i != pictures.end(); ++i) {
            FLAC::Picture *p = *i;
            [artwork addObject:artwork_entry(@"flac_picture", inspection_string(p->mimeType()),
                @(static_cast<uint32_t>(p->type())), inspection_string(p->description()),
                [NSNull null], p->data())];
        }
    }
    if (source.mp4) append_mp4_artwork(source, artwork);

    NSDictionary *result = @{@"artist": inspection_string(source.get_string(0)),
        @"album": inspection_string(source.get_string(1)), @"year": @(source.year()),
        @"title": inspection_string(source.get_string(2)), @"comment": inspection_string(source.get_string(3)),
        @"tempo": @(source.tempo()), @"initial_key": inspection_string(source.key()),
        @"properties": properties, @"unsupported_data": unsupported,
        @"id3_frames": id3_frames, @"artwork": artwork};
    NSError *error = nil;
    NSData *json = [NSJSONSerialization dataWithJSONObject:result options:0 error:&error];
    require(json != nil && error == nil, "cannot serialize source inspection");
    require(receive(context, static_cast<const uint8_t *>(json.bytes), json.length) == 0, "inspection receiver failed");
}

void move_frames(ID3 *source, ID3 *destination) {
    if (!source || !destination) return;
    while (!source->frameList().isEmpty()) {
        Frame *f = source->frameList().front();
        ByteVector id = f->frameID();
        if (id == "TCON") { destination->setGenre(source->genre()); source->removeFrame(f, true); continue; }
        Comment *comment = id == "COMM" ? dynamic_cast<Comment *>(f) : nullptr;
        GEOB *geob = id == "GEOB" ? dynamic_cast<GEOB *>(f) : nullptr;
        if ((comment && comment->description() == "iTunNORM") || (geob && geob->description() == "Serato Autotags")) {
            source->removeFrame(f, true); continue;
        }
        source->removeFrame(f, false);
        destination->addFrame(f);
    }
}
void copy_mp4_to_id3(MP4::Tag *source, ID3 *destination) {
    Tag::duplicate(source, destination, true);
    Frame *comment = first_frame(destination, "COMM");
    if (auto *c = dynamic_cast<Comment *>(comment)) c->setLanguage("eng");
    auto &items = source->itemListMap();
    if (items.contains("trkn")) {
        MP4::Item::IntPair p = items["trkn"].toIntPair();
        set_text(destination, "TRCK", String::number(p.first) + "/" + String::number(p.second));
    }
    if (items.contains("tmpo") && items["tmpo"].toInt()) set_text(destination, "TBPM", String::number(items["tmpo"].toInt()));
    if (items.contains("covr")) {
        auto art = items["covr"].toCoverArtList();
        for (auto i = art.begin(); i != art.end(); ++i) {
            auto *p = new Picture; p->setPicture(i->data());
            p->setMimeType(i->format() == MP4::CoverArt::JPEG ? "image/jpeg" : "image/png");
            destination->addFrame(p);
        }
    }
    if (items.contains("cpil") && items["cpil"].toInt()) set_text(destination, "TCMP", "1");
}

void direct_copy(int operation, const char *src, const char *dst) {
    FileStream source_stream(src, true), destination_stream(dst, false);
    require(source_stream.isOpen() && destination_stream.isOpen(), "cannot open source or destination tag file");
    switch (operation) {
    case PN_COPY_ID3: {
        MPEG::File source(&source_stream, ID3v2::FrameFactory::instance(), false);
        MPEG::File destination(&destination_stream, ID3v2::FrameFactory::instance(), false);
        require(source.isValid() && destination.isValid(), "invalid MPEG source or destination");
        ID3v1::Tag *v1 = source.ID3v1Tag(false);
        bool has_v1 = v1 && !v1->isEmpty();
        if (has_v1) Tag::duplicate(v1, destination.ID3v1Tag(true), true);
        ID3 *v2 = source.ID3v2Tag(false);
        bool has_v2 = v2 && !v2->isEmpty();
        if (has_v2) move_frames(v2, destination.ID3v2Tag(true));
        if (has_v1 || has_v2) require(destination.save(has_v1 ? 3 : 2, true, 4, false), "MPEG direct save failed");
        break;
    }
    case PN_COPY_MP4: {
        MP4::File source(&source_stream, false), destination(&destination_stream, false);
        require(source.isValid() && destination.isValid() && source.tag() && destination.tag(), "invalid MP4 source or destination");
        auto &s = source.tag()->itemListMap(); auto &d = destination.tag()->itemListMap(); d.clear();
        for (auto i = s.begin(); i != s.end(); ++i)
            if (i->first != "----:com.apple.iTunes:iTunNORM" && i->first != "----:com.apple.iTunes:iTunSMPB") d.insert(i->first, i->second);
        require(destination.save(), "MP4 direct save failed"); break;
    }
    case PN_COPY_WAV: {
        WavFile source(&source_stream), destination(&destination_stream);
        require(source.isValid() && destination.isValid(), "invalid WAV source or destination");
        bool has_id3 = source.tag() && !source.tag()->isEmpty() && destination.tag();
        if (has_id3) move_frames(source.tag(), destination.tag());
        ByteVector list = source.firstList();
        if (!list.isEmpty()) destination.setFirstList(list);
        if (has_id3 || !list.isEmpty())
            require(destination.save(static_cast<RIFF::WAV::File::TagTypes>(has_id3 ? 3 : 2), true, 4), "WAV direct save failed");
        break;
    }
    case PN_COPY_AIFF: {
        RIFF::AIFF::File source(&source_stream, false), destination(&destination_stream, false);
        require(source.isValid() && source.tag(), "invalid AIFF source");
        if (source.tag()->isEmpty()) break;
        require(destination.isValid() && destination.tag(), "invalid AIFF destination");
        move_frames(source.tag(), destination.tag()); require(destination.save(), "AIFF direct save failed"); break;
    }
    case PN_COPY_FLAC: {
        FLAC::File source(&source_stream, ID3v2::FrameFactory::instance(), false);
        FLAC::File destination(&destination_stream, ID3v2::FrameFactory::instance(), false);
        require(source.isValid() && destination.isValid(), "invalid FLAC source or destination");
        std::string rejected;
        auto *x = source.xiphComment(false);
        if (x && !x->isEmpty()) {
            auto *d = destination.xiphComment(true); require(d, "cannot create destination Xiph comment");
            PropertyMap unsupported = d->setProperties(x->properties());
            for (auto i = unsupported.begin(); i != unsupported.end(); ++i) {
                if (!rejected.empty()) rejected += ", ";
                rejected += i->first.to8Bit(true);
            }
        }
        auto *v1 = source.ID3v1Tag(false);
        if (v1 && !v1->isEmpty()) {
            auto *d = destination.ID3v1Tag(true);
            if (d) Tag::duplicate(v1, d, true); else rejected += " [ID3v1 creation failed]";
        }
        auto *v2 = source.ID3v2Tag(false);
        if (v2 && !v2->isEmpty()) {
            auto *d = destination.ID3v2Tag(true);
            if (d) move_frames(v2, d); else rejected += " [ID3v2 creation failed]";
        }
        require(destination.save(), "FLAC direct save failed");
        if (!rejected.empty()) throw std::runtime_error("FLAC fields or tags not copied: " + rejected);
        break;
    }
    case PN_COPY_MP4_TO_ID3: {
        MP4::File source(&source_stream, false);
        MPEG::File destination(&destination_stream, ID3v2::FrameFactory::instance(), false);
        require(source.isValid() && destination.isValid() && source.tag(), "invalid MP4 or MPEG container");
        ID3 *d = destination.ID3v2Tag(true); require(d, "cannot create destination ID3 tag");
        copy_mp4_to_id3(source.tag(), d); require(destination.save(), "MP4-to-ID3 direct save failed"); break;
    }
    default: throw std::runtime_error("unknown direct-copy operation");
    }
}
} // namespace

// KVC uses the same seven-key NSArray and dictionary copy as MIKTag. In
// particular Foundation controls dictionary enumeration/setter order.
@interface PNTagValues : NSObject { @public State *state; }
- (instancetype)initWithState:(State *)s;
- (NSString *)artist; - (void)setArtist:(NSString *)s;
- (NSString *)album; - (void)setAlbum:(NSString *)s;
- (NSString *)title; - (void)setTitle:(NSString *)s;
- (NSString *)comment; - (void)setComment:(NSString *)s;
- (long long)year; - (void)setYear:(long long)n;
- (long long)tempo; - (void)setTempo:(long long)n;
- (NSString *)initialKey; - (void)setInitialKey:(NSString *)s;
@end
@implementation PNTagValues
- (instancetype)initWithState:(State *)s { self = [super init]; if (self) state = s; return self; }
- (NSString *)artist { return to_ns(state->get_string(0)); }
- (void)setArtist:(NSString *)s { state->put_string(0, from_ns(s)); }
- (NSString *)album { return to_ns(state->get_string(1)); }
- (void)setAlbum:(NSString *)s { state->put_string(1, from_ns(s)); }
- (NSString *)title { return to_ns(state->get_string(2)); }
- (void)setTitle:(NSString *)s { state->put_string(2, from_ns(s)); }
- (NSString *)comment { return to_ns(state->get_string(3)); }
- (void)setComment:(NSString *)s { state->put_string(3, from_ns(s)); }
- (long long)year { return state->year(); }
- (void)setYear:(long long)n { state->set_year(n); }
- (long long)tempo { return state->tempo(); }
- (void)setTempo:(long long)n { state->set_tempo(n); }
- (NSString *)initialKey { return to_ns(state->key()); }
- (void)setInitialKey:(NSString *)s { state->set_key(from_ns(s)); }
@end

namespace {
template<class F> int guarded(char *error, size_t cap, F &&fn) noexcept {
    if (error && cap) error[0] = '\0';
    @try {
        try { @autoreleasepool { fn(); } return 0; }
        catch (const std::exception &e) { if (error && cap) std::snprintf(error, cap, "%s", e.what()); return 1; }
        catch (...) { if (error && cap) std::snprintf(error, cap, "unknown C++ exception"); return 1; }
    } @catch (NSException *e) {
        if (error && cap) std::snprintf(error, cap, "Objective-C exception: %s", e.reason.UTF8String ?: "unknown");
        return 1;
    }
}
}
extern "C" uint32_t pn_tag_bridge_abi(void) { return 5; }
extern "C" int pn_tag_inspect(int format, const char *path, pn_tag_receive_inspection receive, void *context, char *error, size_t cap) {
    return guarded(error, cap, [&] { inspect_source(format, path, receive, context); });
}
extern "C" int pn_tag_read_marker(int format, const char *path, pn_tag_receive_marker receive, void *context, char *error, size_t cap) {
    return guarded(error, cap, [&] { read_marker(format, path, receive, context); });
}
extern "C" int pn_tag_copy_direct(int operation, const char *src, const char *dst, char *error, size_t cap) {
    return guarded(error, cap, [&] { require(src && dst, "null source or destination path"); direct_copy(operation, src, dst); });
}
extern "C" int pn_tag_copy_generic(int sf, int df, const char *src, const char *dst, pn_tag_serialize_marker serialize, pn_tag_rewrite_wav rewrite_wav, void *context, char *error, size_t cap) {
    return guarded(error, cap, [&] {
        require(src && dst, "null source or destination path");
        State source(sf, src, false), destination(df, dst, true);
        PNTagValues *from = [[PNTagValues alloc] initWithState:&source];
        PNTagValues *to = [[PNTagValues alloc] initWithState:&destination];
        NSArray *keys = @[@"artist", @"album", @"year", @"title", @"comment", @"tempo", @"initialKey"];
        [to setValuesForKeysWithDictionary:[from dictionaryWithValuesForKeys:keys]];
        if (destination.supports_images() && source.has_images()) destination.set_images(source.images());
        set_marker(destination, serialize, context);
        require(destination.save(), "generic metadata save failed");
        if (destination.wav) require(rewrite_wav && rewrite_wav(context, dst) == 0, "WAV marker rewrite failed");
    });
}
extern "C" int pn_tag_save_marker(int df, const char *dst, pn_tag_serialize_marker serialize, pn_tag_rewrite_wav rewrite_wav, void *context, char *error, size_t cap) {
    return guarded(error, cap, [&] {
        require(dst, "null destination path");
        State destination(df, dst, true);
        set_marker(destination, serialize, context);
        require(destination.save(), "marker tag save failed");
        if (destination.wav) require(rewrite_wav && rewrite_wav(context, dst) == 0, "WAV marker rewrite failed");
    });
}
