/* mfx.h is the umbrella for the C API (session, encode, dispatcher, caps) but
 * does NOT pull in mfxmemory.h — that one carries the surface-sharing import
 * API (mfxMemoryInterface / mfxSurfaceD3D11Tex2D, ONEVPL_EXPERIMENTAL) the
 * zero-copy experiment needs, so include it explicitly. */
#include <vpl/mfx.h>
#include <vpl/mfxmemory.h>
