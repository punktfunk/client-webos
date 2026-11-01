#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <wayland-client.h>
#include <wayland-webos-foreign-client-protocol.h>

typedef struct {
	const void *buf;
	uint32_t luma_offset, chroma_offset, length, stride, x, y, width, height, pixel_format;
} hal_vo_image;

typedef struct {
	uint32_t type, value, a2, a3, a4;
} dile_vo_config;

static int (*vo_open)(int, int);
static int (*vo_close)(int);
static int (*vo_config)(int, dile_vo_config);
static int (*hal_display)(int, hal_vo_image *);
static int vo_ch = -1;

static struct wl_event_queue *queue;
static struct wl_display *wrapper;
static struct wl_webos_foreign *foreign;
static struct wl_webos_exported *exported;
static struct wl_webos_imported *imported;
static struct wl_compositor *compositor;
static char window_id[128];

static void on_global_remove(void *data, struct wl_registry *reg, uint32_t name)
{
	(void)data;
	(void)reg;
	(void)name;
}

static void on_window_id(void *data, struct wl_webos_exported *exp, const char *id, uint32_t type)
{
	(void)data;
	(void)exp;
	(void)type;
	snprintf(window_id, sizeof(window_id), "%s", id);
}

static const struct wl_webos_exported_listener exported_listener = { on_window_id };

static struct wl_region *region(struct wl_compositor *c, int w, int h)
{
	struct wl_region *r = wl_compositor_create_region(c);
	wl_region_add(r, 0, 0, w, h);
	return r;
}

static void on_global(void *data, struct wl_registry *reg, uint32_t name, const char *iface, uint32_t ver)
{
	(void)data;
	(void)ver;
	if (!strcmp(iface, "wl_webos_foreign"))
		foreign = wl_registry_bind(reg, name, &wl_webos_foreign_interface, 1);
	else if (!strcmp(iface, "wl_compositor"))
		compositor = wl_registry_bind(reg, name, &wl_compositor_interface, 1);
}

static const struct wl_registry_listener registry_listener = { on_global, on_global_remove };

int pf_vo_punch(struct wl_display *display, struct wl_surface *surface, const char *context, int w, int h)
{
	if (imported)
		return 0;
	queue = wl_display_create_queue(display);
	wrapper = wl_proxy_create_wrapper(display);
	wl_proxy_set_queue((struct wl_proxy *)wrapper, queue);
	struct wl_registry *reg = wl_display_get_registry(wrapper);
	wl_registry_add_listener(reg, &registry_listener, NULL);
	wl_display_roundtrip_queue(display, queue);
	if (!foreign || !compositor)
		return -1;
	exported = wl_webos_foreign_export_element(foreign, surface, WL_WEBOS_FOREIGN_WEBOS_EXPORTED_TYPE_VIDEO_OBJECT);
	wl_webos_exported_add_listener(exported, &exported_listener, NULL);
	wl_webos_exported_set_exported_window(exported, region(compositor, w, h), region(compositor, w, h));
	for (int i = 0; i < 50 && !window_id[0]; i++)
		wl_display_roundtrip_queue(display, queue);
	if (!window_id[0])
		return -2;
	imported = wl_webos_foreign_import_element(foreign, window_id, WL_WEBOS_FOREIGN_WEBOS_EXPORTED_TYPE_VIDEO_OBJECT);
	wl_proxy_marshal((struct wl_proxy *)imported, 2, context);
	wl_display_roundtrip_queue(display, queue);
	return 0;
}

const char *pf_vo_window_id(void)
{
	return window_id;
}

int pf_vo_open(int uhd)
{
	if (vo_ch >= 0)
		return 0;
	dlopen("libPmLogLib.so.3", RTLD_NOW | RTLD_GLOBAL);
	void *dile = dlopen("libdile_photo.so.1", RTLD_NOW);
	void *hal = dlopen("libhal_photo.so.2", RTLD_NOW);
	if (!dile || !hal)
		return -1;
	vo_open = dlsym(dile, "DILE_VO_Open");
	vo_close = dlsym(dile, "DILE_VO_Close");
	vo_config = dlsym(dile, "DILE_VO_Config");
	hal_display = dlsym(hal, "HAL_VO_DisplayPicture");
	if (!vo_open || !vo_close || !vo_config || !hal_display)
		return -2;
	int r = vo_open(0, uhd ? 2 : 1);
	if (r)
		return r;
	vo_ch = 0;
	dile_vo_config c = { 4, 13, 0, 0, 0 };
	vo_config(vo_ch, c);
	return 0;
}

int pf_vo_show(const uint8_t *luma, const uint8_t *chroma, uint32_t stride, uint32_t w, uint32_t h)
{
	if (vo_ch < 0)
		return -1;
	hal_vo_image img = { luma, 0, (uint32_t)(chroma - luma), stride * h * 3 / 2, stride, 0, 0, w, h, 13 };
	return hal_display(vo_ch, &img);
}

void pf_vo_close(void)
{
	if (vo_ch >= 0) {
		vo_close(vo_ch);
		vo_ch = -1;
	}
	if (imported) {
		wl_webos_imported_detach_punchthrough(imported);
		wl_webos_imported_destroy(imported);
		imported = NULL;
	}
	if (exported) {
		wl_webos_exported_destroy(exported);
		exported = NULL;
	}
	window_id[0] = 0;
}
