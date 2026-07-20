/*
 * This file is part of mpv.
 *
 * mpv is free software; you can redistribute it and/or
 * modify it under the terms of the GNU Lesser General Public
 * License as published by the Free Software Foundation; either
 * version 2.1 of the License, or (at your option) any later version.
 */

#include "options/m_config.h"
#include "options/options.h"
#include "sd.h"

#undef OPT_BASE_STRUCT
#define OPT_BASE_STRUCT struct mp_sub_filter_opts

const struct m_sub_options mp_sub_filter_opts = {
    .opts = (const struct m_option[]){
        {"sdh", OPT_BOOL(sub_filter_SDH)},
        {"sdh-harder", OPT_BOOL(sub_filter_SDH_harder)},
        {"sdh-enclosures", OPT_STRINGLIST(sub_filter_SDH_enclosures)},
        {"regex-enable", OPT_BOOL(rf_enable)},
        {"regex-plain", OPT_BOOL(rf_plain)},
        {"regex", OPT_STRINGLIST(rf_items)},
        {"jsre", OPT_STRINGLIST(jsre_items)},
        {"regex-warn", OPT_BOOL(rf_warn)},
        {0}
    },
    .size = sizeof(OPT_BASE_STRUCT),
    .defaults = &(OPT_BASE_STRUCT){
        .sub_filter_SDH_enclosures = (char *[]){
            "()",
            "[]",
            "\uFF08\uFF09",
            NULL
        },
        .rf_enable = true,
    },
    .change_flags = UPDATE_SUB_FILT,
};
