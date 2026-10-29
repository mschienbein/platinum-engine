#ifndef PLATINUM_TAGLIB_BRIDGE_H
#define PLATINUM_TAGLIB_BRIDGE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Caller-selected, separately built open-source TagLib 1.11.1 candidate bridge.
 * These IDs are an ABI between this bridge and src/tags.rs, not PN type codes. */
enum pn_tag_format { PN_TAG_MP3 = 1, PN_TAG_MP4 = 2, PN_TAG_WAV = 3,
                     PN_TAG_AIFF = 4, PN_TAG_FLAC = 5 };
enum pn_tag_copy { PN_COPY_ID3 = 1, PN_COPY_MP4 = 2, PN_COPY_WAV = 3,
                   PN_COPY_AIFF = 4, PN_COPY_FLAC = 5, PN_COPY_MP4_TO_ID3 = 6 };

/* ABI-5 result codes for bridge operations:
 * 0: success; 1: ordinary failure, including path/open/read/write errors,
 * destination container errors, callbacks and malformed metadata;
 * 2: portable-only refusal of artwork requiring or potentially requiring
 * Apple image transcoding; 3: an opened source tag container is unavailable
 * or invalid for the requested format. A status-3 copy does not commit to the
 * destination or invoke callbacks. Destination validation takes precedence
 * over status 3; callers may separately save a marker to a valid destination.
 * Source I/O errors return 1 even if TagLib finds a valid container; unlike
 * the legacy bridge, a directory presented as MP3 is an error, not success.
 * The CLI admits only regular files. */

typedef struct pn_tag_payload {
    const uint8_t *bytes; /* may be NULL only when length is zero */
    size_t length;
    int present; /* 0 is original nil tagData, not an empty payload */
} pn_tag_payload;

/* Synchronous callback; the owner retains returned bytes until the next call
 * or the enclosing bridge call returns. Return nonzero on serialization error.
 * The bridge never retains the callback, context, or returned bytes. */
typedef int (*pn_tag_serialize_marker)(void *context, pn_tag_payload *payload);

/* Called synchronously after WAV tag save, before its FileStream is closed.
 * The original marker reader observes the still-open stream's on-disk bytes. */
typedef int (*pn_tag_rewrite_wav)(void *context, const char *path);

/* Called once on successful inspection. A present empty payload has present=1
 * and length=0. The borrowed bytes are valid only during this callback.
 * Return nonzero if the receiver cannot retain the result. */
typedef int (*pn_tag_receive_marker)(void *context, const uint8_t *bytes,
                                     size_t length, int present);
int pn_tag_read_marker(int format, const char *path, pn_tag_receive_marker receive,
                       void *context, char *error, size_t error_capacity);

/* Inspect the source read-only and synchronously lend a UTF-8 JSON document
 * describing the generic fields, TagLib PropertyMap/unsupportedData, ordered
 * ID3 frames and embedded artwork. The receiver must copy or consume bytes
 * before returning; nonzero rejects the result. */
typedef int (*pn_tag_receive_inspection)(void *context, const uint8_t *json,
                                         size_t length);
int pn_tag_inspect(int format, const char *path, pn_tag_receive_inspection receive,
                   void *context, char *error, size_t error_capacity);

uint32_t pn_tag_bridge_abi(void);
/* Each operation returns zero on success and writes a bounded diagnostic on
 * failure. Path bytes are filesystem representation, not necessarily UTF-8.
 * Direct copy commits before the caller separately opens for marker saving. */
int pn_tag_copy_direct(int operation, const char *source, const char *destination,
                       char *error, size_t error_capacity);
/* Generic copy and marker setter share one open destination and one save.
 * The callback is invoked at the actual setter boundary, twice for FLAC only
 * when an ID3v2 tag exists. WAV uses rewrite_wav before the native destination
 * is closed, not after this operation returns. */
int pn_tag_copy_generic(int source_format, int destination_format,
                        const char *source, const char *destination,
                        pn_tag_serialize_marker serialize, pn_tag_rewrite_wav rewrite_wav, void *context,
                        char *error, size_t error_capacity);
int pn_tag_save_marker(int destination_format, const char *destination,
                       pn_tag_serialize_marker serialize, pn_tag_rewrite_wav rewrite_wav, void *context,
                       char *error, size_t error_capacity);

/* ABI-5 optional extension: opt-in byte-for-byte artwork copy (portable
 * bridge only; the macOS bridge never exports these symbols). The six
 * functions above are unchanged and behave exactly as before; a caller that
 * wants the extension looks the symbols up optionally and treats a missing
 * pn_tag_bridge_features as "no extension".
 *
 * PN_TAG_COPY_ARTWORK_BYTES makes the bridge store each embedded picture's
 * encoded bytes unchanged (FLAC PICTURE, ID3 APIC, MP4 covr, in source order)
 * where the reference bridge would re-encode them as PNG through
 * NSImage/ImageIO (generic copies) or, for FLAC direct copies, drop them.
 * Type, description and, for FLAC sources, the picture geometry are carried
 * over where the destination has a place for them (MP4 has neither; its
 * pictures count as front covers elsewhere). The MIME type is always the
 * canonical type of the validated bytes: a source label that disagrees, such
 * as "image/jpg", is corrected. Pictures with zero bytes are skipped, as in
 * the reference. The result is therefore NOT byte-identical to the macOS
 * reference for any file whose pictures were copied;
 * PN_TAG_RESULT_ARTWORK_COPIED reports exactly that case. Direct copies that
 * already move pictures unchanged in the reference (ID3, MP4, AIFF,
 * MP4 -> ID3) are untouched by the flag.
 *
 * Status 2 under the flag means: a picture is not a structurally valid
 * JPEG/PNG/GIF/BMP/WebP image, or the destination cannot store it as-is
 * (MP4 covr holds only JPEG/PNG/GIF/BMP; a FLAC metadata block holds at most
 * 16 MiB - 1; an ID3v2 tag - MP3, AIFF or WAV - holds at most 268435455
 * bytes of frames and padding, counted exactly as rendered: pictures, their
 * frame headers, MIME types and descriptions in the encoding they are
 * written in, every other frame already in the destination tag, the
 * PlatinumNotes marker frame and TagLib's padding). The refusal happens
 * before any destination write or, for everything but the marker frame,
 * before any callback; only a tag that the marker frame itself pushes over
 * the limit is refused after the serialize callback ran, still before any
 * write.
 * With the flag, WAV is an ID3-capable container like MP3 and AIFF in generic
 * copies: pictures from any source are stored as APIC frames in a WAV
 * destination's ID3v2 tag, and a WAV source's APIC frames are the pictures
 * copied to any destination. Without the flag (flags == 0 and the five
 * original exports) generic copies never move a picture into or out of a WAV,
 * as in the reference; direct copies are unchanged either way. A picture
 * reaches the finished WAV only if the caller's rewrite_wav keeps the ID3
 * chunk TagLib saved last: the reference marker rewrite drops it when the
 * file is rewritten while the stream is still open, the engine's opt-in keeps
 * it in its Rust marker rewrite; the bridge itself does not rewrite.
 * flags with unknown bits fail with status 1. result_flags may be NULL; it is
 * set to 0 on entry and to the OR of PN_TAG_RESULT_* only when the call
 * returns 0. */
enum { PN_TAG_FEATURE_ARTWORK_COPY = 1 };
enum { PN_TAG_COPY_ARTWORK_BYTES = 1 };
enum { PN_TAG_RESULT_ARTWORK_COPIED = 1 };
uint32_t pn_tag_bridge_features(void);
int pn_tag_copy_direct_ex(unsigned flags, int operation, const char *source, const char *destination,
                          unsigned *result_flags, char *error, size_t error_capacity);
int pn_tag_copy_generic_ex(unsigned flags, int source_format, int destination_format,
                           const char *source, const char *destination,
                           pn_tag_serialize_marker serialize, pn_tag_rewrite_wav rewrite_wav, void *context,
                           unsigned *result_flags, char *error, size_t error_capacity);

#ifdef __cplusplus
}
#endif
#endif
