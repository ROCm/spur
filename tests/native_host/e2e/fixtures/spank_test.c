// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

// SPANK plugin used by the e2e suite.
//
// The spank_* symbols are resolved at dlopen time from the spurd executable's
// dynamic symbol table, so the plugin declares them extern and links against
// nothing. Behaviour is driven by plugstack args:
//
//   var=NAME        env var to inject (default SPANK_TEST_VAR)
//   value=VALUE     value to inject (default injected)
//   trace=PATH      append one line per hook to PATH
//   fail=HOOK       return non-zero from HOOK (init|task_init|task_exit)

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct spank *spank_t;

#define ESPANK_SUCCESS 0
#define ESPANK_ERROR 1

#define S_JOB_UID 0
#define S_JOB_ID 2

extern int spank_setenv(spank_t spank, const char *var, const char *val, int overwrite);
extern int spank_getenv(spank_t spank, const char *var, char *buf, int len);
extern int spank_get_item(spank_t spank, int item, void *val);

// Slurm requires these; Spur does not read them, but a real plugin has them
// and their presence keeps the fixture honest about the ABI.
const char plugin_name[] = "spank_test";
const char plugin_type[] = "spank";
const unsigned int plugin_version = 1;

static const char *arg_value(int ac, char **argv, const char *key)
{
    size_t klen = strlen(key);
    for (int i = 0; i < ac; i++) {
        if (argv[i] && strncmp(argv[i], key, klen) == 0 && argv[i][klen] == '=')
            return argv[i] + klen + 1;
    }
    return NULL;
}

static void trace(int ac, char **argv, const char *hook)
{
    const char *path = arg_value(ac, argv, "trace");
    if (!path)
        return;
    FILE *f = fopen(path, "a");
    if (!f)
        return;
    fprintf(f, "%s ac=%d\n", hook, ac);
    fclose(f);
}

static int should_fail(int ac, char **argv, const char *hook)
{
    const char *fail = arg_value(ac, argv, "fail");
    return fail != NULL && strcmp(fail, hook) == 0;
}

int slurm_spank_init(spank_t sp, int ac, char **argv)
{
    trace(ac, argv, "init");
    if (should_fail(ac, argv, "init"))
        return ESPANK_ERROR;

    if (spank_setenv(sp, "SPANK_TEST_INIT", "1", 1) != ESPANK_SUCCESS)
        return ESPANK_ERROR;
    return ESPANK_SUCCESS;
}

int slurm_spank_task_init(spank_t sp, int ac, char **argv)
{
    trace(ac, argv, "task_init");
    if (should_fail(ac, argv, "task_init"))
        return ESPANK_ERROR;

    const char *var = arg_value(ac, argv, "var");
    const char *value = arg_value(ac, argv, "value");
    if (spank_setenv(sp, var ? var : "SPANK_TEST_VAR", value ? value : "injected", 1)
        != ESPANK_SUCCESS)
        return ESPANK_ERROR;

    // Overwrite=0 must not clobber a value the plugin just set.
    spank_setenv(sp, "SPANK_TEST_NOCLOBBER", "first", 1);
    spank_setenv(sp, "SPANK_TEST_NOCLOBBER", "second", 0);

    unsigned int job_id = 0;
    if (spank_get_item(sp, S_JOB_ID, &job_id) == ESPANK_SUCCESS) {
        char buf[32];
        snprintf(buf, sizeof(buf), "%u", job_id);
        spank_setenv(sp, "SPANK_TEST_JOB_ID", buf, 1);
    }

    unsigned int uid = 0;
    if (spank_get_item(sp, S_JOB_UID, &uid) == ESPANK_SUCCESS) {
        char buf[32];
        snprintf(buf, sizeof(buf), "%u", uid);
        spank_setenv(sp, "SPANK_TEST_UID", buf, 1);
    }

    // Read back through the handle to prove setenv and getenv share state.
    char echo[64] = {0};
    if (spank_getenv(sp, "SPANK_TEST_INIT", echo, (int)sizeof(echo)) == ESPANK_SUCCESS)
        spank_setenv(sp, "SPANK_TEST_SAW_INIT", echo, 1);

    return ESPANK_SUCCESS;
}

int slurm_spank_task_exit(spank_t sp, int ac, char **argv)
{
    (void)sp;
    trace(ac, argv, "task_exit");
    if (should_fail(ac, argv, "task_exit"))
        return ESPANK_ERROR;
    return ESPANK_SUCCESS;
}
