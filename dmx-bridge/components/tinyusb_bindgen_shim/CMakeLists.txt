# TinyUSB's osal_freertos.h reaches FreeRTOS through
# TU_INCLUDE_PATH(CFG_TUSB_OS_INC_PATH, ...), and that define is private to the
# tinyusb component. esp-idf-sys's bindgen pass never receives it, so the macro
# expands to bare "FreeRTOS.h" and binding generation fails. These shims make
# the bare names resolvable without putting the whole FreeRTOS kernel include
# directory on the global search path.
idf_component_register(INCLUDE_DIRS "include" REQUIRES freertos)
