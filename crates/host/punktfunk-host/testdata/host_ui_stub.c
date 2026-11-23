// The smallest menu bar UI library for the host's loader (hostsys/app_loop.rs). Each call appends
// a line to <config_dir>/ui-stub.log. -DSTUB_ABI=<n> and -DSTUB_PRELAUNCH=<n> set the answers;
// -DSTUB_NO_RUN leaves pf_host_ui_run out.
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <unistd.h>

#ifndef STUB_ABI
#define STUB_ABI 1
#endif
#ifndef STUB_PRELAUNCH
#define STUB_PRELAUNCH 0
#endif

typedef struct pf_host_ui_context {
    uint32_t abi_version;
    const char *config_dir;
    const char *version;
    uint8_t launched_by_login_agent;
} pf_host_ui_context;

static void note(const pf_host_ui_context *ctx, const char *call) {
    char path[4096];
    snprintf(path, sizeof path, "%s/ui-stub.log", ctx->config_dir);
    FILE *f = fopen(path, "a");
    if (!f) {
        return;
    }
    fprintf(f, "%s abi=%u version=%s agent=%u main=%d pid=%d\n", call, ctx->abi_version,
            ctx->version, ctx->launched_by_login_agent, pthread_main_np(), getpid());
    fclose(f);
}

uint32_t pf_host_ui_abi_version(void) { return STUB_ABI; }

int32_t pf_host_ui_prelaunch(const pf_host_ui_context *ctx) {
    note(ctx, "prelaunch");
    return STUB_PRELAUNCH;
}

#ifndef STUB_NO_RUN
void pf_host_ui_run(const pf_host_ui_context *ctx) {
    note(ctx, "run");
    for (;;) {
        pause();
    }
}
#endif
