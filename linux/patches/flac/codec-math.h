/* Redirect only the codec's own calls, never process-global libm symbols. */
#ifndef PN_FLAC_CODEC_REDIRECTS_H
#define PN_FLAC_CODEC_REDIRECTS_H
#include <math.h>
#include "pn_codec_math.h"
#define cosf pn_codec_cosf
#define log pn_codec_log
#define frexp pn_codec_frexp
#define exp pn_codec_exp
#define lround pn_codec_lround
#endif
