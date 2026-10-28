/* Redirect only the codec's own calls, never process-global libm symbols. */
#ifndef PN_LAME_CODEC_REDIRECTS_H
#define PN_LAME_CODEC_REDIRECTS_H
#include <math.h>
#include "pn_codec_math.h"
#define atan pn_codec_atan
#define cos pn_codec_cos
#define sin pn_codec_sin
#define tan pn_codec_tan
#define exp pn_codec_exp
#define exp2 pn_codec_exp2
#define log pn_codec_log
#define log10 pn_codec_log10
#define log10f pn_codec_log10f
#define powf pn_codec_powf
#define pow pn_codec_pow
#endif
