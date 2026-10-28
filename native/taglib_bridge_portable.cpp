// Portable implementation of the ABI in taglib_bridge.h.
// Generic copies that may require NSImage/ImageIO artwork transcoding refuse
// before saving or invoking callbacks with status 2 and a diagnostic. Direct
// copies, marker operations, inspection, and generic copies to WAV do not
// transcode artwork. Nonempty but undecodable artwork also refuses because
// NSImage validity cannot be determined on Linux. Empty artwork is skipped by
// the reference and its corresponding destination artwork is cleared.
// No Apple frameworks are used.
// To export only the ABI when linking static TagLib, use
// -Wl,--version-script=tools/portable/bridge/bridge.map on ELF, or
// -Wl,-exported_symbols_list,tools/portable/bridge/bridge.exports on Darwin.
// Foundation dictionary and JSON ordering model the macOS 27.2 arm64
// reference behavior; they are not a cross-version Foundation contract.
#include "taglib_bridge.h"
#include <aifffile.h>
#include <apefooter.h>
#include <apetag.h>
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
#include <cerrno>
#include <climits>
#include <cstdio>
#include <cstring>
#include <memory>
#include <optional>
#include <stdexcept>
#include <string>
#include <vector>
#include <algorithm>
#include <array>
#include <cstdint>
#include <limits>
#include <utility>

using namespace TagLib;
namespace {
using ID3 = ID3v2::Tag;
using Frame = ID3v2::Frame;
using Picture = ID3v2::AttachedPictureFrame;
using Comment = ID3v2::CommentsFrame;
using GEOB = ID3v2::GeneralEncapsulatedObjectFrame;

struct SourceContainerUnavailable : std::runtime_error {
    explicit SourceContainerUnavailable(const char *message) : std::runtime_error(message) {}
};


// Record I/O failures even when TagLib can continue parsing the source.
class CheckedInputStream : public FileStream {
    mutable bool failed = false;
    void observe() const { if (errno != 0) failed = true; }
public:
    explicit CheckedInputStream(const char *path) : FileStream(path, true) {}
    bool had_io_error() const { return failed; }
    ByteVector readBlock(unsigned long length) override {
        const long start = tell();
        const long end = this->length();
        errno = 0;
        ByteVector result = FileStream::readBlock(length);
        observe();
        if (start >= 0 && end > start) {
            const auto remaining = static_cast<unsigned long>(end - start);
            const auto expected = length < remaining ? length : remaining;
            if (result.size() < expected) failed = true;
        }
        return result;
    }
    void seek(long offset, Position position = Beginning) override {
        errno = 0;
        FileStream::seek(offset, position);
        if (errno == EINVAL && offset < 0) {
            // TagLib probes for trailing tags even on files shorter than the
            // tag; its legacy stream ignores seeks before the start of file.
            errno = 0;
            const long base = position == Beginning ? 0 :
                              position == Current ? FileStream::tell() : FileStream::length();
            if (base >= 0 && offset < -base) { errno = 0; return; }
            failed = true;
            return;
        }
        observe();
    }
    long tell() const override {
        errno = 0;
        long result = FileStream::tell();
        if (result < 0) failed = true;
        observe();
        return result;
    }
    long length() override {
        errno = 0;
        long result = FileStream::length();
        if (result < 0) failed = true;
        observe();
        return result;
    }
};

void check_source(const CheckedInputStream &input) {
    if (input.had_io_error()) throw std::runtime_error("source tag stream I/O failed");
}

void require_source(bool condition, const char *message, const CheckedInputStream &input) {
    check_source(input);
    if (!condition) throw SourceContainerUnavailable(message);
}

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

// NSString's UTF8String is NUL-terminated; invalid UTF-8 becomes nil.
String foundation_copy_string(const String &value);

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

    State(int kind, const char *path, bool write, bool normalize_id3 = true)
        : format(kind), stream(write ? new FileStream(path, false) : new CheckedInputStream(path)) {
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
        if (write) require(file->isValid(), "invalid source or destination container");
        else require_source(file->isValid(), "invalid source tag container",
                            *static_cast<CheckedInputStream *>(stream.get()));
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
        bool available = (kind != PN_TAG_MP4 || m4) && (kind != PN_TAG_WAV || v2) &&
                         (kind != PN_TAG_AIFF || v2) && (kind != PN_TAG_FLAC || xiph);
        if (write) require(available, "tag object unavailable");
        else require_source(available, "source tag object unavailable",
                            *static_cast<CheckedInputStream *>(stream.get()));
    }
    void check_source() const { ::check_source(*static_cast<CheckedInputStream *>(stream.get())); }
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
    bool images_are_empty() const {
        if (m4) {
            auto &items = m4->itemListMap();
            if (items.contains("covr")) {
                auto images = items["covr"].toCoverArtList();
                for (auto i = images.begin(); i != images.end(); ++i)
                    if (!i->data().isEmpty()) return false;
            }
        } else if (flac) {
            auto images = flac->pictureList();
            for (auto i = images.begin(); i != images.end(); ++i)
                if (!(*i)->data().isEmpty()) return false;
        } else if (v2 && !wav) {
            const auto &frames = v2->frameList("APIC");
            for (auto i = frames.begin(); i != frames.end(); ++i) {
                Picture *picture = dynamic_cast<Picture *>(*i);
                if (picture && !picture->picture().isEmpty()) return false;
            }
        }
        return true;
    }
    void clear_images() {
        if (flac) flac->removePictures();
        else if (v2) v2->removeFrames("APIC");
        if (m4) m4->itemListMap().insert("covr", MP4::Item(MP4::CoverArtList()));
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

std::optional<ByteVector> wav_marker(WavFile &wav, bool collect = true) {
    const long length = wav.length();
    require(length >= 12, "truncated WAV header");
    wav.seek(0);
    ByteVector header = wav.readBlock(12);
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
        wav.seek(static_cast<long>(position));
        ByteVector chunk = wav.readBlock(8);
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
            ByteVector bytes = wav.readBlock(n);
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

void validate_aiff(RIFF::AIFF::File &aiff, bool source, const CheckedInputStream *input = nullptr) {
    auto check = [source, input](bool condition, const char *message) {
        if (source) require_source(condition, message, *input);
        else require(condition, message);
    };
    check(aiff.length() >= 12, "truncated AIFF header");
    aiff.seek(0);
    ByteVector header = aiff.readBlock(12);
    check(header.size() == 12 && header.mid(0, 4) == "FORM" &&
          (header.mid(8, 4) == "AIFF" || header.mid(8, 4) == "AIFC"), "invalid AIFF header");
    const auto *p = reinterpret_cast<const uint8_t *>(header.data());
    const uint32_t size = (uint32_t(p[4]) << 24) | (uint32_t(p[5]) << 16) |
                          (uint32_t(p[6]) << 8) | uint32_t(p[7]);
    check(size >= 4 && uint64_t(size) + 8 <= static_cast<uint64_t>(aiff.length()), "invalid AIFF FORM length");
}

// A sync-like word may occur inside another format, notably Ogg. Check every
// frame through the end of audio, excluding recognized trailing ID3v1/APE tags.
bool valid_mpeg_destination(MPEG::File &file) {
    long end = file.length();
    if (end < 0) return false;
    if (file.hasID3v1Tag()) {
        if (end < 128) return false;
        end -= 128;
    }
    if (file.hasAPETag()) {
        APE::Tag *tag = file.APETag(false);
        if (!tag || !tag->footer()) return false;
        const unsigned size = tag->footer()->completeTagSize();
        if (size > static_cast<unsigned long>(end)) return false;
        end -= size;
    }
    long offset = file.firstFrameOffset();
    while (offset >= 0 && offset < end) {
        MPEG::Header first(&file, offset, false);
        if (first.isValid()) {
            const auto version = first.version();
            const int layer = first.layer(), rate = first.sampleRate();
            while (offset < end) {
                MPEG::Header frame(&file, offset, false);
                const int length = frame.frameLength();
                if (!frame.isValid() || frame.version() != version || frame.layer() != layer ||
                    frame.sampleRate() != rate || length <= 0 || length > end - offset) return false;
                offset += length;
            }
            return offset == end;
        }
        if (offset >= end - 1) break;
        offset = file.nextFrameOffset(offset + 1);
    }
    return false;
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
        require_source(found, "invalid or truncated MPEG audio",
                       *static_cast<CheckedInputStream *>(source.stream.get()));
    }
    if (source.aiff) validate_aiff(*source.aiff, true, static_cast<CheckedInputStream *>(source.stream.get()));
    source.check_source();
}

void read_marker(int format, const char *path, pn_tag_receive_marker receive, void *context) {
    require(path && receive, "null marker path or receiver");
    State source(format, path, false);
    validate_source(source);
    if (source.flac) {
        auto xiph = flac_marker(source);
        source.check_source();
        if (xiph) {
            require(receive(context, xiph->data(), xiph->size(), 1) == 0, "marker receiver failed");
            return;
        }
    }
    std::optional<ByteVector> value;
    if (source.wav) value = wav_marker(*source.wav);
    else if (source.m4) {
        auto &items = source.m4->itemListMap();
        const String key("----:com.mixedinkey.platinumnotes:data");
        if (items.contains(key)) {
            const MP4::Item item = items[key];
            require(item.atomDataType() == MP4::TypeUTF8, "unsupported MP4 marker atom data type");
            value = item.toStringList().toString(" ").data(String::UTF8);
        }
    } else value = id3_marker(source.v2);
    source.check_source();
    const auto *bytes = value && !value->isEmpty() ? reinterpret_cast<const uint8_t *>(value->data()) : nullptr;
    require(receive(context, bytes, value ? value->size() : 0, value.has_value() ? 1 : 0) == 0, "marker receiver failed");
}


// NSString validates the entire UTF-8 sequence before UTF8String exposes a
// C string. Keep the validation separate from the subsequent NUL truncation.
bool utf16_units(const std::string &input, std::vector<uint16_t> *units = nullptr) {
    for (size_t i = 0; i < input.size();) {
        unsigned char a = static_cast<unsigned char>(input[i++]);
        uint32_t c; unsigned extra;
        if (a < 0x80) { c = a; extra = 0; }
        else if (a >= 0xc2 && a <= 0xdf) { c = a & 0x1f; extra = 1; }
        else if (a >= 0xe0 && a <= 0xef) { c = a & 0x0f; extra = 2; }
        else if (a >= 0xf0 && a <= 0xf4) { c = a & 0x07; extra = 3; }
        else return false;
        if (extra > input.size() - i) return false;
        for (unsigned n = 0; n < extra; ++n) {
            unsigned char b = static_cast<unsigned char>(input[i++]);
            if ((b & 0xc0) != 0x80) return false;
            c = (c << 6) | (b & 0x3f);
        }
        if ((extra == 2 && c < 0x800) || (extra == 3 && c < 0x10000) ||
            (c >= 0xd800 && c <= 0xdfff) || c > 0x10ffff) return false;
        if (units) {
            if (c <= 0xffff) units->push_back(static_cast<uint16_t>(c));
            else {
                c -= 0x10000;
                units->push_back(static_cast<uint16_t>(0xd800 + (c >> 10)));
                units->push_back(static_cast<uint16_t>(0xdc00 + (c & 0x3ff)));
            }
        }
    }
    return true;
}
bool foundation_utf8(std::string &text) {
    if (!utf16_units(text)) return false;
    // initWithBytes:length:encoding: consumes one leading UTF-8 BOM.
    if (text.size() >= 3 && text.compare(0, 3, "\xef\xbb\xbf", 3) == 0)
        text.erase(0, 3);
    return true;
}


String foundation_copy_string(const String &value) {
    if (value.isNull()) return String::null;
    std::string utf8 = value.to8Bit(true);
    if (!foundation_utf8(utf8)) return String::null;
    return String(utf8.c_str(), String::UTF8);
}

std::string inspection_text(const String &value) {
    require(!value.isNull(), "null TagLib property name or value");
    std::string text = value.to8Bit(true);
    require(foundation_utf8(text), "cannot encode TagLib string as UTF-8");
    return text;
}

void json_string(std::string &out, const std::string &text) {
    require(utf16_units(text), "cannot serialize source inspection");
    static const char hex[] = "0123456789abcdef";
    out.push_back('"');
    for (unsigned char c : text) {
        switch (c) {
        case '"': out += "\\\""; break;
        case '\\': out += "\\\\"; break;
        case '/': out += "\\/"; break;
        case '\b': out += "\\b"; break;
        case '\f': out += "\\f"; break;
        case '\n': out += "\\n"; break;
        case '\r': out += "\\r"; break;
        case '\t': out += "\\t"; break;
        default:
            if (c < 0x20) {
                out += "\\u00"; out.push_back(hex[c >> 4]); out.push_back(hex[c & 15]);
            } else out.push_back(static_cast<char>(c));
        }
    }
    out.push_back('"');
}

void json_tag_string(std::string &out, const String &s) {
    if (s.isNull()) out += "null";
    else json_string(out, inspection_text(s));
}

// The macOS 27.2 reference uses 64-bit NSString hashes and a mutable
// Foundation dictionary's ascending hash-bucket enumeration. Property keys
// are inserted in TagLib PropertyMap order, including rehashes as it grows.
uint64_t foundation_hash(const std::string &text) {
    std::vector<uint16_t> u;
    require(utf16_units(text, &u), "cannot encode TagLib string as UTF-8");
    size_t length = u.size();
    std::vector<uint16_t> sample;
    if (length > 96) {
        sample.insert(sample.end(), u.begin(), u.begin() + 32);
        sample.insert(sample.end(), u.begin() + length / 2 - 16, u.begin() + length / 2 + 16);
        sample.insert(sample.end(), u.end() - 32, u.end());
    } else sample.swap(u);
    uint64_t h = length;
    size_t i = 0;
    for (; i + 4 <= sample.size(); i += 4) {
        uint32_t group = sample[i];
        group = group * 257u + sample[i + 1];
        group = group * 257u + sample[i + 2];
        group = group * 257u + sample[i + 3];
        h = h * UINT64_C(67503105) + group;
    }
    for (; i < sample.size(); ++i) h = h * 257 + sample[i];
    return h + (h << (length & 31));
}

class PropertyDictionary {
    struct Entry { std::string key, value; uint64_t hash; };
    std::vector<Entry> entries;
    std::vector<size_t> slots;
    size_t stage = 0;
    static constexpr size_t empty = std::numeric_limits<size_t>::max();
    static constexpr size_t sizes[] = {
        0, 3, 7, 13, 23, 41, 71, 127, 191, 251, 383, 631, 1087, 1723,
        2803, 4523, 7351, 11959, 19447, 31231, 50683, 81919, 132607
    };
    static constexpr size_t capacities[] = {
        0, 3, 6, 11, 19, 32, 52, 85, 118, 155, 237, 390, 672, 1065,
        1732, 2795, 4543, 7391, 12019, 19302, 31324, 50629, 81956
    };
    void place(size_t index) {
        size_t position = entries[index].hash % slots.size();
        while (slots[position] != empty) position = (position + 1) % slots.size();
        slots[position] = index;
    }
    void grow() {
        require(stage + 1 < sizeof sizes / sizeof sizes[0], "too many inspection properties");
        std::vector<size_t> old = std::move(slots);
        slots.assign(sizes[++stage], empty);
        for (size_t index : old) if (index != empty) place(index);
    }
public:
    void add(std::string key, std::string value) {
        uint64_t hash = foundation_hash(key);
        if (!slots.empty()) {
            size_t position = hash % slots.size();
            size_t start = position;
            while (slots[position] != empty) {
                Entry &entry = entries[slots[position]];
                if (entry.hash == hash && entry.key == key) {
                    entry.value = std::move(value);
                    return;
                }
                position = (position + 1) % slots.size();
                if (position == start) break;
            }
        }
        bool must_grow = entries.size() == capacities[stage];
        bool grow_before_insert = must_grow && entries.size() == slots.size();
        if (grow_before_insert) grow();
        entries.push_back({std::move(key), std::move(value), hash});
        place(entries.size() - 1);
        if (must_grow && !grow_before_insert) grow();
    }
    void append_json(std::string &out) const {
        out.push_back('{'); bool first = true;
        for (size_t index : slots) if (index != empty) {
            if (!first) out.push_back(',');
            first = false;
            json_string(out, entries[index].key); out.push_back(':'); out += entries[index].value;
        }
        out.push_back('}');
    }
};

uint32_t rotr(uint32_t x, unsigned n) { return (x >> n) | (x << (32 - n)); }
std::string sha256_bytes(const ByteVector &bytes) {
    static constexpr uint32_t k[64] = {
        0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,0x923f82a4,0xab1c5ed5,
        0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,
        0xe49b69c1,0xefbe4786,0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,
        0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,0x06ca6351,0x14292967,
        0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,
        0xa2bfe8a1,0xa81a664b,0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
        0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,0x5b9cca4f,0x682e6ff3,
        0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2
    };
    uint32_t h[] = {0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19};
    auto block = [&](const unsigned char *p) {
        uint32_t w[64];
        for (int i = 0; i < 16; ++i)
            w[i] = (uint32_t(p[4*i]) << 24) | (uint32_t(p[4*i+1]) << 16) |
                   (uint32_t(p[4*i+2]) << 8) | p[4*i+3];
        for (int i = 16; i < 64; ++i) {
            uint32_t a = rotr(w[i-15],7) ^ rotr(w[i-15],18) ^ (w[i-15] >> 3);
            uint32_t b = rotr(w[i-2],17) ^ rotr(w[i-2],19) ^ (w[i-2] >> 10);
            w[i] = w[i-16] + a + w[i-7] + b;
        }
        uint32_t a=h[0], b=h[1], c=h[2], d=h[3], e=h[4], f=h[5], g=h[6], q=h[7];
        for (int i = 0; i < 64; ++i) {
            uint32_t t1=q+(rotr(e,6)^rotr(e,11)^rotr(e,25))+((e&f)^(~e&g))+k[i]+w[i];
            uint32_t t2=(rotr(a,2)^rotr(a,13)^rotr(a,22))+((a&b)^(a&c)^(b&c));
            q=g; g=f; f=e; e=d+t1; d=c; c=b; b=a; a=t1+t2;
        }
        h[0]+=a; h[1]+=b; h[2]+=c; h[3]+=d; h[4]+=e; h[5]+=f; h[6]+=g; h[7]+=q;
    };
    const auto *data = reinterpret_cast<const unsigned char *>(bytes.data());
    size_t size = bytes.size(), pos = 0;
    while (size - pos >= 64) { block(data + pos); pos += 64; }
    unsigned char last[128] = {};
    size_t remaining = size - pos;
    if (remaining) std::memcpy(last, data + pos, remaining);
    last[remaining] = 0x80;
    size_t final_size = remaining < 56 ? 64 : 128;
    uint64_t bits = uint64_t(size) * 8;
    for (unsigned i = 0; i < 8; ++i) last[final_size - 1 - i] = static_cast<unsigned char>(bits >> (8 * i));
    block(last); if (final_size == 128) block(last + 64);
    static const char hex[] = "0123456789abcdef";
    std::string result; result.reserve(64);
    for (uint32_t v : h) for (int i = 7; i >= 0; --i) result.push_back(hex[(v >> (i * 4)) & 15]);
    return result;
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

std::string artwork_entry(const char *container, const String &mime,
                          std::optional<uint32_t> type, const String &description,
                          std::optional<uint32_t> format, const ByteVector &bytes) {
    std::string out = "{\"sha256\":";
    json_string(out, sha256_bytes(bytes));
    out += ",\"byte_count\":" + std::to_string(bytes.size()) + ",\"format_code\":";
    out += format ? std::to_string(*format) : "null";
    out += ",\"container\":"; json_string(out, container);
    out += ",\"picture_type\":"; out += type ? std::to_string(*type) : "null";
    out += ",\"description\":"; json_tag_string(out, description);
    out += ",\"mime_type\":"; json_tag_string(out, mime);
    out.push_back('}'); return out;
}

ByteVector decode_base64(const uint8_t *input, size_t length) {
    auto digit = [](uint8_t c) -> int {
        if (c >= 'A' && c <= 'Z') return c - 'A';
        if (c >= 'a' && c <= 'z') return c - 'a' + 26;
        if (c >= '0' && c <= '9') return c - '0' + 52;
        if (c == '+') return 62;
        if (c == '/') return 63;
        return -1;
    };
    std::vector<char> result;
    for (size_t i = 0; i < length;) {
        require(length - i >= 4, "invalid Xiph artwork Base64");
        uint8_t a = input[i], b = input[i+1], c = input[i+2], d = input[i+3];
        int x = digit(a), y = digit(b), z = digit(c), w = digit(d);
        bool all_padding = a == '=' && b == '=' && c == '=' && d == '=';
        require(all_padding || (x >= 0 && y >= 0 && (z >= 0 || c == '=') &&
                (w >= 0 || d == '=') && (c != '=' || d == '=')), "invalid Xiph artwork Base64");
        if (all_padding) x = y = 0;
        result.push_back(static_cast<char>((x << 2) | (y >> 4)));
        if (c != '=') {
            result.push_back(static_cast<char>((y << 4) | (z >> 2)));
            if (d != '=') result.push_back(static_cast<char>((z << 6) | w));
        }
        i += 4;
        if (d == '=') {
            while (i < length && input[i] == '=') ++i;
            require(i == length, "invalid Xiph artwork Base64");
        }
        require(result.size() <= UINT_MAX, "invalid Xiph artwork Base64");
    }
    return ByteVector(result.empty() ? "" : result.data(), static_cast<uint>(result.size()));
}

void append_xiph_picture(std::vector<std::string> &out, const uint8_t *encoded, size_t size, bool flac_picture) {
    ByteVector bytes = decode_base64(encoded, size);
    if (!flac_picture) {
        out.push_back(artwork_entry("flac_xiph_coverart", String::null, {}, String::null, {}, bytes));
        return;
    }
    const auto *p = reinterpret_cast<const uint8_t *>(bytes.data());
    size_t cursor = 0;
    auto number = [&]() -> uint32_t {
        require(cursor <= bytes.size() && bytes.size() - cursor >= 4, "truncated Xiph picture");
        uint32_t n = (uint32_t(p[cursor]) << 24) | (uint32_t(p[cursor + 1]) << 16) |
                     (uint32_t(p[cursor + 2]) << 8) | uint32_t(p[cursor + 3]);
        cursor += 4; return n;
    };
    (void)number();
    uint32_t mime_size = number();
    require(mime_size <= bytes.size() - cursor, "truncated Xiph picture MIME"); cursor += mime_size;
    uint32_t description_size = number();
    require(description_size <= bytes.size() - cursor, "truncated Xiph picture description"); cursor += description_size;
    for (int i = 0; i < 4; ++i) (void)number();
    uint32_t image_size = number();
    require(image_size <= bytes.size() - cursor, "truncated Xiph picture bytes");
    FLAC::Picture picture;
    require(picture.parse(bytes), "invalid Xiph picture block");
    out.push_back(artwork_entry("flac_xiph_picture", picture.mimeType(),
        static_cast<uint32_t>(picture.type()), picture.description(), {}, picture.data()));
}

size_t append_xiph_artwork_and_count_native_pictures(State &s, std::vector<std::string> &out) {
    require(!s.v2 || s.v2->header(), "FLAC ID3 header unavailable");
    const long start = s.v2 ? static_cast<long>(s.v2->header()->completeTagSize()) : 0;
    const long length = s.flac->length();
    const long magic = s.flac->find(ByteVector("fLaC"), start);
    require(length >= 4 && magic >= 0 && magic <= length - 4, "invalid FLAC metadata location");
    long position = magic + 4;
    bool last = false, comment_seen = false;
    size_t pictures = 0;
    while (!last) {
        require(position <= length - 4, "truncated FLAC metadata header");
        s.flac->seek(position);
        ByteVector header = s.flac->readBlock(4);
        require(header.size() == 4, "cannot read FLAC metadata header");
        const auto *h = reinterpret_cast<const uint8_t *>(header.data());
        last = (h[0] & 0x80) != 0;
        unsigned kind = h[0] & 0x7f;
        if (kind == 6) ++pictures;
        uint32_t size = (uint32_t(h[1]) << 16) | (uint32_t(h[2]) << 8) | uint32_t(h[3]);
        position += 4;
        require(static_cast<long>(size) <= length - position, "truncated FLAC metadata block");
        if (kind == 4 && !comment_seen) {
            comment_seen = true;
            ByteVector raw = s.flac->readBlock(size);
            require(raw.size() == size, "truncated FLAC Vorbis comment");
            const auto *p = reinterpret_cast<const uint8_t *>(raw.data());
            size_t cursor = 0;
            auto number = [&]() -> uint32_t {
                require(cursor <= size && size - cursor >= 4, "truncated FLAC Vorbis comment");
                uint32_t n = uint32_t(p[cursor]) | (uint32_t(p[cursor+1]) << 8) |
                             (uint32_t(p[cursor+2]) << 16) | (uint32_t(p[cursor+3]) << 24);
                cursor += 4; return n;
            };
            uint32_t vendor_size = number();
            require(vendor_size <= size - cursor, "truncated FLAC Vorbis vendor"); cursor += vendor_size;
            uint32_t count = number();
            for (uint32_t i = 0; i < count; ++i) {
                uint32_t field_size = number();
                require(field_size <= size - cursor, "truncated FLAC Vorbis field");
                const uint8_t *field = p + cursor;
                if (xiph_key(field, field_size, "METADATA_BLOCK_PICTURE="))
                    append_xiph_picture(out, field + 23, field_size - 23, true);
                else if (xiph_key(field, field_size, "COVERART="))
                    append_xiph_picture(out, field + 9, field_size - 9, false);
                cursor += field_size;
            }
        }
        position += size;
    }
    return pictures;
}

void append_mp4_artwork(State &s, std::vector<std::string> &out) {
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
                length = extended.toLongLong(); header_size = 16;
            } else if (length == 0) length = static_cast<uint64_t>(end - pos);
            require(length >= static_cast<uint64_t>(header_size + 8) &&
                    length <= static_cast<uint64_t>(end - pos), "invalid MP4 cover data length");
            ByteVector type_locale = s.mp4->readBlock(8);
            require(type_locale.size() == 8, "truncated MP4 cover type");
            uint32_t type = type_locale.toUInt();
            uint64_t size = length - header_size - 8;
            require(size <= UINT_MAX, "MP4 artwork exceeds TagLib byte limit");
            ByteVector bytes = s.mp4->readBlock(static_cast<unsigned long>(size));
            require(bytes.size() == size, "truncated MP4 artwork");
            String mime = String::null;
            if (type == MP4::TypeJPEG) mime = "image/jpeg";
            else if (type == MP4::TypePNG) mime = "image/png";
            else if (type == MP4::TypeGIF) mime = "image/gif";
            else if (type == MP4::TypeBMP) mime = "image/bmp";
            out.push_back(artwork_entry("mp4_covr", mime, {}, String::null, type, bytes));
            pos += static_cast<long>(length);
        }
    }
}

void append_array(std::string &out, const std::vector<std::string> &items) {
    out.push_back('[');
    for (size_t i = 0; i < items.size(); ++i) { if (i) out.push_back(','); out += items[i]; }
    out.push_back(']');
}

void inspect_source(int format, const char *path, pn_tag_receive_inspection receive, void *context) {
    require(path && receive, "null inspection path or receiver");
    State source(format, path, false, false);
    validate_source(source);
    if (source.wav) (void)wav_marker(*source.wav, false);
    PropertyMap map = source_properties(source);
    PropertyDictionary properties;
    for (auto i = map.begin(); i != map.end(); ++i) {
        std::string key = inspection_text(i->first), values = "[";
        bool first = true;
        for (auto v = i->second.begin(); v != i->second.end(); ++v) {
            if (!first) values.push_back(',');
            first = false; json_string(values, inspection_text(*v));
        }
        values.push_back(']'); properties.add(std::move(key), std::move(values));
    }
    std::vector<std::string> unsupported, frames, artwork;
    const StringList &unmapped = map.unsupportedData();
    for (auto i = unmapped.begin(); i != unmapped.end(); ++i) {
        std::string value; json_string(value, inspection_text(*i)); unsupported.push_back(std::move(value));
    }
    bool disk_id3 = has_disk_id3(source);
    if (disk_id3) {
        require(source.v2 != nullptr, "ID3v2 tag unavailable");
        const auto &list = source.v2->frameList();
        for (auto i = list.begin(); i != list.end(); ++i) {
            ByteVector id = (*i)->frameID();
            for (uint n = 0; n < id.size(); ++n)
                require(static_cast<unsigned char>(id[n]) < 128, "ID3 frame identifier is not ASCII");
            std::string frame = "{\"id\":";
            json_string(frame, std::string(id.data(), id.size()));
            frame += ",\"byte_count\":" + std::to_string((*i)->size()) + "}";
            frames.push_back(std::move(frame));
            if (id == "APIC") {
                auto *picture = dynamic_cast<Picture *>(*i);
                require(picture != nullptr, "unrecognized ID3 attached picture");
                artwork.push_back(artwork_entry("id3v2", picture->mimeType(),
                    static_cast<uint32_t>(picture->type()), picture->description(), {}, picture->picture()));
            }
        }
    }
    if (source.flac) {
        auto pictures = source.flac->pictureList();
        require(append_xiph_artwork_and_count_native_pictures(source, artwork) == pictures.size(),
                "FLAC picture block could not be parsed");
        for (auto i = pictures.begin(); i != pictures.end(); ++i) {
            FLAC::Picture *p = *i;
            artwork.push_back(artwork_entry("flac_picture", p->mimeType(),
                static_cast<uint32_t>(p->type()), p->description(), {}, p->data()));
        }
    }
    if (source.mp4) append_mp4_artwork(source, artwork);
    std::string json = "{\"artist\":";
    json_tag_string(json, source.get_string(0));
    json += ",\"id3_frames\":";
    if (disk_id3) append_array(json, frames); else json += "null";
    json += ",\"artwork\":"; append_array(json, artwork);
    json += ",\"initial_key\":"; json_tag_string(json, source.key());
    json += ",\"title\":"; json_tag_string(json, source.get_string(2));
    json += ",\"tempo\":" + std::to_string(source.tempo());
    json += ",\"album\":"; json_tag_string(json, source.get_string(1));
    json += ",\"year\":" + std::to_string(source.year());
    json += ",\"comment\":"; json_tag_string(json, source.get_string(3));
    json += ",\"properties\":"; properties.append_json(json);
    json += ",\"unsupported_data\":"; append_array(json, unsupported); json.push_back('}');
    source.check_source();
    require(receive(context, reinterpret_cast<const uint8_t *>(json.data()), json.size()) == 0,
            "inspection receiver failed");
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
    CheckedInputStream source_stream(src);
    FileStream destination_stream(dst, false);
    require(source_stream.isOpen() && destination_stream.isOpen(), "cannot open source or destination tag file");
    switch (operation) {
    case PN_COPY_ID3: {
        MPEG::File source(&source_stream, ID3v2::FrameFactory::instance(), false);
        MPEG::File destination(&destination_stream, ID3v2::FrameFactory::instance(), false);
        require(destination.isValid(), "invalid MPEG destination");
        if (!source.isValid()) {
            require(!destination_stream.readOnly(), "cannot write destination tag file");
            require(valid_mpeg_destination(destination), "invalid or truncated MPEG destination");
        }
        require_source(source.isValid(), "invalid MPEG source", source_stream);
        ID3v1::Tag *v1 = source.ID3v1Tag(false);
        ID3 *v2 = source.ID3v2Tag(false);
        bool has_v1 = v1 && !v1->isEmpty();
        bool has_v2 = v2 && !v2->isEmpty();
        check_source(source_stream);
        if (has_v1) Tag::duplicate(v1, destination.ID3v1Tag(true), true);
        if (has_v2) move_frames(v2, destination.ID3v2Tag(true));
        if (has_v1 || has_v2) require(destination.save(has_v1 ? 3 : 2, true, 4, false), "MPEG direct save failed");
        break;
    }
    case PN_COPY_MP4: {
        MP4::File source(&source_stream, false), destination(&destination_stream, false);
        require(destination.isValid() && destination.tag(), "invalid MP4 destination");
        if (!source.isValid() || !source.tag()) require(!destination_stream.readOnly(), "cannot write destination tag file");
        require_source(source.isValid() && source.tag(), "invalid MP4 source", source_stream);
        auto &s = source.tag()->itemListMap(); auto &d = destination.tag()->itemListMap(); d.clear();
        for (auto i = s.begin(); i != s.end(); ++i)
            if (i->first != "----:com.apple.iTunes:iTunNORM" && i->first != "----:com.apple.iTunes:iTunSMPB") d.insert(i->first, i->second);
        require(destination.save(), "MP4 direct save failed"); break;
    }
    case PN_COPY_WAV: {
        WavFile source(&source_stream), destination(&destination_stream);
        require(destination.isValid(), "invalid WAV destination");
        if (!source.isValid()) {
            require(!destination_stream.readOnly(), "cannot write destination tag file");
            (void)wav_marker(destination, false);
        }
        require_source(source.isValid(), "invalid WAV source", source_stream);
        bool has_id3 = source.tag() && !source.tag()->isEmpty() && destination.tag();
        ByteVector list = source.firstList();
        check_source(source_stream);
        if (has_id3) move_frames(source.tag(), destination.tag());
        if (!list.isEmpty()) destination.setFirstList(list);
        if (has_id3 || !list.isEmpty())
            require(destination.save(static_cast<RIFF::WAV::File::TagTypes>(has_id3 ? 3 : 2), true, 4), "WAV direct save failed");
        break;
    }
    case PN_COPY_AIFF: {
        RIFF::AIFF::File source(&source_stream, false), destination(&destination_stream, false);
        if (!source.isValid() || !source.tag()) {
            require(destination.isValid() && destination.tag() && !destination_stream.readOnly(), "invalid AIFF destination");
            validate_aiff(destination, false);
            require_source(false, "invalid AIFF source", source_stream);
        }
        check_source(source_stream);
        if (source.tag()->isEmpty()) break;
        require(destination.isValid() && destination.tag(), "invalid AIFF destination");
        move_frames(source.tag(), destination.tag()); require(destination.save(), "AIFF direct save failed"); break;
    }
    case PN_COPY_FLAC: {
        FLAC::File source(&source_stream, ID3v2::FrameFactory::instance(), false);
        FLAC::File destination(&destination_stream, ID3v2::FrameFactory::instance(), false);
        require(destination.isValid(), "invalid FLAC destination");
        if (!source.isValid()) require(!destination_stream.readOnly(), "cannot write destination tag file");
        require_source(source.isValid(), "invalid FLAC source", source_stream);
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
        check_source(source_stream);
        require(destination.save(), "FLAC direct save failed");
        if (!rejected.empty()) throw std::runtime_error("FLAC fields or tags not copied: " + rejected);
        break;
    }
    case PN_COPY_MP4_TO_ID3: {
        MP4::File source(&source_stream, false);
        MPEG::File destination(&destination_stream, ID3v2::FrameFactory::instance(), false);
        require(destination.isValid(), "invalid MPEG destination");
        if (!source.isValid() || !source.tag()) {
            require(!destination_stream.readOnly() && destination.ID3v2Tag(true), "invalid MPEG destination");
            require(valid_mpeg_destination(destination), "invalid or truncated MPEG destination");
            require_source(false, "invalid MP4 source", source_stream);
        }
        check_source(source_stream);
        ID3 *d = destination.ID3v2Tag(true); require(d, "cannot create destination ID3 tag");
        copy_mp4_to_id3(source.tag(), d); require(destination.save(), "MP4-to-ID3 direct save failed"); break;
    }
    default: throw std::runtime_error("unknown direct-copy operation");
    }
}
} // namespace

namespace {
struct ArtworkTranscode : std::runtime_error {
    ArtworkTranscode() : std::runtime_error(
        "portable tag bridge refuses artwork requiring Apple NSImage/ImageIO transcoding") {}
};

template<class F> int guarded(char *error, size_t cap, F &&fn) noexcept {
    if (error && cap) error[0] = '\0';
    try { fn(); return 0; }
    catch (const SourceContainerUnavailable &e) {
        if (error && cap) std::snprintf(error, cap, "%s", e.what());
        return 3;
    }
    catch (const ArtworkTranscode &e) {
        if (error && cap) std::snprintf(error, cap, "%s", e.what());
        return 2;
    } catch (const std::exception &e) {
        if (error && cap) std::snprintf(error, cap, "%s", e.what());
        return 1;
    } catch (...) {
        if (error && cap) std::snprintf(error, cap, "unknown C++ exception");
        return 1;
    }
}
}

#if defined(__GNUC__) || defined(__clang__)
#define PN_TAG_EXPORT __attribute__((visibility("default")))
#else
#define PN_TAG_EXPORT
#endif
extern "C" PN_TAG_EXPORT uint32_t pn_tag_bridge_abi(void) { return 5; }
extern "C" PN_TAG_EXPORT int pn_tag_inspect(int format, const char *path, pn_tag_receive_inspection receive,
                                               void *context, char *error, size_t cap) {
    return guarded(error, cap, [&] { inspect_source(format, path, receive, context); });
}
extern "C" PN_TAG_EXPORT int pn_tag_read_marker(int format, const char *path, pn_tag_receive_marker receive,
                                                   void *context, char *error, size_t cap) {
    return guarded(error, cap, [&] { read_marker(format, path, receive, context); });
}
extern "C" PN_TAG_EXPORT int pn_tag_copy_direct(int operation, const char *src, const char *dst,
                                                  char *error, size_t cap) {
    return guarded(error, cap, [&] { require(src && dst, "null source or destination path"); direct_copy(operation, src, dst); });
}
extern "C" PN_TAG_EXPORT int pn_tag_copy_generic(int sf, int df, const char *src, const char *dst,
        pn_tag_serialize_marker serialize, pn_tag_rewrite_wav rewrite_wav, void *context, char *error, size_t cap) {
    return guarded(error, cap, [&] {
        require(src && dst, "null source or destination path");
        std::optional<State> source_storage;
        try { source_storage.emplace(sf, src, false); }
        catch (const SourceContainerUnavailable &) {
            State destination(df, dst, true);
            require(!destination.stream->readOnly(), "cannot write destination tag file");
            if (destination.mp3) require(valid_mpeg_destination(*destination.mp3), "invalid or truncated MPEG destination");
            if (destination.wav) (void)wav_marker(*destination.wav, false);
            if (destination.aiff) validate_aiff(*destination.aiff, false);
            if (destination.flac) (void)flac_marker(destination);
            throw;
        }
        State &source = *source_storage;
        State destination(df, dst, true);
        bool copy_images = destination.supports_images() && source.has_images();
        if (copy_images && !source.images_are_empty()) {
            source.check_source();
            throw ArtworkTranscode();
        }
        // dictionaryWithValuesForKeys: obtains all values before any setter.
        String artist = foundation_copy_string(source.get_string(0));
        String album = foundation_copy_string(source.get_string(1));
        long long year = source.year();
        String title = foundation_copy_string(source.get_string(2));
        String comment = foundation_copy_string(source.get_string(3));
        long long tempo = source.tempo();
        String key = foundation_copy_string(source.key());
        source.check_source();
        // Observed enumeration order for this exact seven-key dictionary.
        destination.put_string(0, artist);
        destination.put_string(2, title);
        destination.set_key(key);
        destination.set_tempo(tempo);
        destination.put_string(1, album);
        destination.set_year(year);
        destination.put_string(3, comment);
        if (copy_images) destination.clear_images();
        set_marker(destination, serialize, context);
        require(destination.save(), "generic metadata save failed");
        if (destination.wav) require(rewrite_wav && rewrite_wav(context, dst) == 0, "WAV marker rewrite failed");
    });
}
extern "C" PN_TAG_EXPORT int pn_tag_save_marker(int df, const char *dst, pn_tag_serialize_marker serialize,
        pn_tag_rewrite_wav rewrite_wav, void *context, char *error, size_t cap) {
    return guarded(error, cap, [&] {
        require(dst, "null destination path");
        State destination(df, dst, true);
        set_marker(destination, serialize, context);
        require(destination.save(), "marker tag save failed");
        if (destination.wav) require(rewrite_wav && rewrite_wav(context, dst) == 0, "WAV marker rewrite failed");
    });
}
