#ifndef PN_CODEC_MATH_H
#define PN_CODEC_MATH_H

/* These explicit entry points do not replace or interpose system libm. */
#ifdef __cplusplus
extern "C" {
#endif

float pn_codec_expf(float);
float pn_codec_exp2f(float);
float pn_codec_exp10f(float);
float pn_codec_logf(float);
float pn_codec_log2f(float);
float pn_codec_log10f(float);
float pn_codec_sinf(float);
float pn_codec_cosf(float);
float pn_codec_floorf(float);
float pn_codec_ceilf(float);
float pn_codec_roundf(float);
float pn_codec_truncf(float);
float pn_codec_sqrtf(float);
float pn_codec_atan2f(float, float);
float pn_codec_fmodf(float, float);
float pn_codec_powf(float, float);

double pn_codec_log(double);
double pn_codec_log10(double);
double pn_codec_pow(double, double);
double pn_codec_exp(double);
double pn_codec_exp2(double);
double pn_codec_exp10(double);
double pn_codec_sin(double);
double pn_codec_cos(double);
double pn_codec_tan(double);
double pn_codec_atan(double);
double pn_codec_logb(double);
double pn_codec_sinh(double);
double pn_codec_cosh(double);
double pn_codec_tanh(double);
double pn_codec_asinh(double);
double pn_codec_log1p(double);
double pn_codec_floor(double);
double pn_codec_ceil(double);
double pn_codec_round(double);
double pn_codec_trunc(double);
double pn_codec_sqrt(double);
double pn_codec_scalbn(double, int);
double pn_codec_powi_f64(double, int);
double pn_codec_frexp(double, int *);
long pn_codec_lround(double);

#ifdef __cplusplus
}
#endif
#endif
