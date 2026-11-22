/* The C ABI between punktfunk-host and libPunktfunkHostUI.dylib, version 1.
 * The host dlopens the library, checks pf_host_ui_abi_version, then calls the two entries on its
 * main thread. A library with another version or a missing symbol is ignored. */
#ifndef PF_HOST_UI_H
#define PF_HOST_UI_H

#include <stdint.h>

#define PF_HOST_UI_ABI_VERSION 1

typedef struct pf_host_ui_context {
    uint32_t abi_version;            /* PF_HOST_UI_ABI_VERSION */
    const char *config_dir;          /* UTF-8, NUL-terminated; valid for the process lifetime */
    const char *version;             /* the host's version string, same lifetime */
    uint8_t launched_by_login_agent; /* 1 when launchd started this instance as the login agent */
} pf_host_ui_context;

/* The ABI version the library implements. */
uint32_t pf_host_ui_abi_version(void);

/* Before serving, only for a launch that is not the login agent. 1: the login agent is
 * registered and this launch was handed to it, so the host exits 0 without serving. */
int32_t pf_host_ui_prelaunch(const pf_host_ui_context *ctx);

/* Runs the app on the process's main thread. Never returns. */
void pf_host_ui_run(const pf_host_ui_context *ctx);

#endif
