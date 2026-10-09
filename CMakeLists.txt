cmake_minimum_required(VERSION 3.20)
project(walleclipse VERSION 0.1.0 LANGUAGES C CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)

if(NOT CMAKE_BUILD_TYPE)
  set(CMAKE_BUILD_TYPE RelWithDebInfo)
endif()

add_compile_options(-Wall -Wextra)

find_package(PkgConfig REQUIRED)
pkg_check_modules(WAYLAND_CLIENT REQUIRED wayland-client)
pkg_check_modules(LIBPNG REQUIRED libpng)
pkg_check_modules(LIBJPEG REQUIRED libjpeg)
pkg_check_modules(LIBWEBP REQUIRED libwebp)

find_program(WAYLAND_SCANNER wayland-scanner REQUIRED)

set(PROTO_DIR ${CMAKE_CURRENT_SOURCE_DIR}/protocols)
set(XDG_OUTPUT_XML /usr/share/wayland-protocols/unstable/xdg-output/xdg-output-unstable-v1.xml)
set(XDG_SHELL_XML /usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml)
set(GEN_DIR ${CMAKE_CURRENT_BINARY_DIR}/generated)
file(MAKE_DIRECTORY ${GEN_DIR})

function(gen_wayland_protocol XML_BASENAME XML_PATH)
  add_custom_command(
    OUTPUT ${GEN_DIR}/${XML_BASENAME}-client.h ${GEN_DIR}/${XML_BASENAME}.c
    COMMAND ${WAYLAND_SCANNER} client-header ${XML_PATH} ${GEN_DIR}/${XML_BASENAME}-client.h
    COMMAND ${WAYLAND_SCANNER} private-code ${XML_PATH} ${GEN_DIR}/${XML_BASENAME}.c
    DEPENDS ${XML_PATH}
    COMMENT "Generating Wayland protocol ${XML_BASENAME}")
endfunction()

gen_wayland_protocol(wlr-layer-shell-unstable-v1 ${PROTO_DIR}/wlr-layer-shell-unstable-v1.xml)
gen_wayland_protocol(xdg-output-unstable-v1 ${XDG_OUTPUT_XML})
gen_wayland_protocol(xdg-shell ${XDG_SHELL_XML})

add_library(wlproto STATIC
  ${GEN_DIR}/wlr-layer-shell-unstable-v1.c
  ${GEN_DIR}/xdg-output-unstable-v1.c
  ${GEN_DIR}/xdg-shell.c)
target_include_directories(wlproto PUBLIC ${GEN_DIR} ${WAYLAND_CLIENT_INCLUDE_DIRS})

add_library(walleclipse_core STATIC
  src/ConfigStore.cpp
  src/ImageLoader.cpp
  src/Preloader.cpp
  src/WaylandBackend.cpp
  src/HyprListener.cpp
  src/MediaDelegate.cpp
  src/Ipc.cpp)
target_include_directories(walleclipse_core PUBLIC
  include ${CMAKE_CURRENT_SOURCE_DIR}/third_party ${GEN_DIR}
  ${WAYLAND_CLIENT_INCLUDE_DIRS}
  ${LIBPNG_INCLUDE_DIRS} ${LIBJPEG_INCLUDE_DIRS} ${LIBWEBP_INCLUDE_DIRS})
target_link_libraries(walleclipse_core PUBLIC
  wlproto ${WAYLAND_CLIENT_LIBRARIES} ${LIBPNG_LIBRARIES}
  ${LIBJPEG_LIBRARIES} ${LIBWEBP_LIBRARIES})

add_executable(walleclipse src/main.cpp)
target_link_libraries(walleclipse PRIVATE walleclipse_core)

include(CTest)
if(BUILD_TESTING)
  add_executable(test_configstore tests/test_configstore.cpp)
  target_link_libraries(test_configstore PRIVATE walleclipse_core)
  add_test(NAME configstore COMMAND test_configstore)

  add_executable(test_image tests/test_image.cpp)
  target_link_libraries(test_image PRIVATE walleclipse_core)
  add_test(NAME image COMMAND test_image)
endif()
