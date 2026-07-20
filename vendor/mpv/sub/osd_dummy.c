/*
 * This file is part of mpv.
 *
 * mpv is free software; you can redistribute it and/or
 * modify it under the terms of the GNU Lesser General Public
 * License as published by the Free Software Foundation; either
 * version 2.1 of the License, or (at your option) any later version.
 */

#include <stddef.h>

#include "misc/bstr.h"
#include "osd.h"
#include "osd_state.h"

void osd_destroy_backend(struct osd_state *osd)
{
    (void)osd;
}

void osd_get_function_sym(char *buffer, size_t buffer_size, int osd_function)
{
    (void)osd_function;
    if (buffer_size)
        buffer[0] = '\0';
}

void osd_mangle_ass(bstr *dst, const char *in, bool replace_newlines)
{
    (void)replace_newlines;
    bstr_xappend(NULL, dst, bstr0(in));
}

void osd_get_text_size(struct osd_state *osd, int *out_screen_h, int *out_font_h)
{
    (void)osd;
    *out_screen_h = 0;
    *out_font_h = 0;
}

void osd_set_external(struct osd_state *osd, struct osd_external_ass *ov)
{
    (void)osd;
    if (ov->out_rc) {
        for (int n = 0; n < 4; n++)
            ov->out_rc[n] = 0;
    }
}

void osd_set_external_remove_owner(struct osd_state *osd, void *owner)
{
    (void)osd;
    (void)owner;
}

struct sub_bitmaps *osd_object_get_bitmaps(struct osd_state *osd,
                                           struct osd_object *obj, int format)
{
    (void)osd;
    (void)format;
    obj->osd_changed = false;
    obj->changed = false;
    return NULL;
}
