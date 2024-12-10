# utils重构实践

这里将阐述详细的coreutils和binutils两个项目的重构实践。

## coreutils

我们选取coreutils-9.0项目进行重构。

首先我们需要安装coreutils的依赖

```shell
$ sudo dnf group install -y "Development Tools"
$ sudo dnf install -y wget texinfo openssl-devel gmp-devel
```

我们还需要安装`bear`工具和`utils-translator`工具，安装方式这里不再赘述。为了更好的展示，我们接下来会使用`jq`，`yq`和`sponge`等工具来处理toml文件，但是这些工具并不是必须的。

然后我们下载并配置coreutils-9.0源码

```shell
$ wget https://mirrors.tuna.tsinghua.edu.cn/gnu/coreutils/coreutils-9.0.tar.xz
$ tar -xvf coreutils-9.0.tar.xz
$ cd coreutils-9.0
$ ./configure --with-openssl --enable-install-program=arch --enable-no-install-program=kill,uptime,stdbuf
```

接下来我们需要使用`bear`工具来获取编译命令

```shell
$ bear -- make
```

此时，当前目录下已经生成了`compile_commands.json`文件。

为了保证转义正常进行，我们先把一些难以处理的文件删除。

```shell
$ jq 'del(.[] | select(.file == "/home/user/coreutils-9.0/src/cksum_pclmul.c")) \
    | del(.[] | select(.file == "/home/user/coreutils-9.0/src/factor.c")) \ 
    | del(.[] | select(.file == "/home/user/coreutils-9.0/src/wc_avx2.c")) \
    ' compile_commands.json >compile_commands2.json
```

使用utils-translator工具进行转换，这里我们需要设置fuzz-depends-level为1，这样可以保证我们提取的依赖关系更加全面。对于不同项目需要针对性设置不同的fuzz-depends-level。

```shell
$ ec2rust-transpile -e compile_commands2.json -o coreutils-rust --detect-binary --emit-binaries --emit-no-lib --fuzz-depends-level 1
```

此时我们已经在当前目录下生成了coreutils-rust文件夹，里面包含了所有的Rust文件。然后我们需要给Rust项目增加一些外部的依赖关系。

```shell
$ cd coreutils-rust
$ cargo add selinux-sys@0.4 -Z sparse-registry
$ sed -i -E 's|(// add unix dependencies below)|\1\n     println!("cargo:rustc-flags=-lgmp -lcrypto");|g' ./build.rs
```

然后我们需要处理的是一些utils-translator工具出现的一些问题，下面是一些常见的问题：

```shell
$ sed -i 's|val != 0.|val != f128::f128::new(0)|g' numfmt.rs  && \
    sed -i 's|f128::f128::new(1.18973149535723176502e+4932)|f128::f128::MAX|g' numfmt.rs && \
    sed -i 's|f128::f128::new(1.18973149535723176502e+4932)|f128::f128::MAX|g' getlimits.rs && \
    sed -i "$(expr $(wc -l < expr.rs) - 50),\$s|args|_args|g" expr.rs && \
    sed -i 's|::std::env::_args()|::std::env::args()|g' expr.rs && \
    sed -i -E ':a;N;$!ba;s|if 0 as libc::c_int != 0 \{\} else \{[[:space:]]*unreachable!\(\);[[:space:]]*\};|unreachable!();|g' paste.rs && \
    sed -i -E ':a;N;$!ba;s|if 0 as libc::c_int != 0 \{\} else \{[[:space:]]*unreachable!\(\);[[:space:]]*\};|unreachable!();|g' ptx.rs && \
    sed -i -E ':a;N;$!ba;s|if 0 as libc::c_int != 0 \{\} else \{[[:space:]]*unreachable!\(\);[[:space:]]*\};|unreachable!();|g' seq.rs && \
    sed -i -E ':a;N;$!ba;s|if 0 as libc::c_int != 0 \{\} else \{[[:space:]]*unreachable!\(\);[[:space:]]*\};|unreachable!();|g' sort.rs && \
    sed -i 's|(\*ap).a.a_longdouble = args.arg::<f128::f128>();|(*ap).a.a_longdouble = args.arg::<libc::c_double>();|g' src/lib/printf_args.rs
```

和一个依赖关系版本的问题

```shell
$ cargo update --package unicode-width --precise 0.1.13 -Z sparse-registry
```

然后我们跑一个`cargo fix`来修复一些`rust`可以解决的问题

```shell
$ cargo fix --broken-code --bins --keep-going -Z unstable-options -Z sparse-registry --allow-no-vcs || true
```

注意因为之前并没有完全解决所有的问题，所以这里可能会有一些报错，但是不影响整体的转换。

此时还需要手动对二进制文件增加一个外部依赖。

```shell
sed -i 's/extern crate libc;/extern crate selinux_sys;\nextern crate libc;/g' ./*.rs
```

此时我们可以运行`cargo build`来构建整个项目。

```shell
$ cargo build --bins --keep-going -Z unstable-options -Z sparse-registry || true
```

至此应当可以预期有81个二进制文件成功构建。

## binutils

同样我们也先安装binutils的项目依赖

```shell
$ sudo dnf group install -y "Development Tools"
$ sudo dnf install -y wget texinfo openssl-devel gmp-devel
```

我们选取binutils-2.37项目进行重构。
```shell
$ wget https://mirrors.tuna.tsinghua.edu.cn/gnu/binutils/binutils-2.37.tar.xz
$ tar -xf binutils-2.37.tar.xz
```

然后我们需要使用`bear`工具来获取编译命令

```shell
$ cd binutils-2.37
$ ./configure \
    --enable-ld \
    --enable-gold=default \
    --with-sysroot=/ \
    --enable-deterministic-archives=no \
    --enable-lto \
    --enable-compressed-debug-sections=none \
    --enable-generate-build-notes=no \
    --enable-targets=x86_64-pep --enable-relro=yes \
    --enable-plugins \
    --enable-shared
$ bear -- make
```

此时，当前目录下已经生成了`compile_commands.json`文件，根据之前依赖关系分析的方法，我们可以使用`filter.sh`脚本来分割不同的文件夹。

```shell
$ ./filter.sh ./compile_commands.json
```

接下来我们进行逐个库的转换。

```shell
# zlib 库
$ ec2rust-transpile -e compile_commands_zlib.json -o ./binutils-rust/zlib
$ cd  ./binutils-rust/zlib
$ yq eval '.package.name="zlib-sys" | .lib.name="zlib_sys"' Cargo.toml -oj | yj -jt | sponge Cargo.toml
$ cargo add f128 -Z sparse-registry
$ cargo build -Z sparse-registry

# bfd 库
$ cd `path/to/binutils-2.37`
$ ec2rust-transpile -e compile_commands_bfd.json -o ./binutils-rust/bfd
$ cd  ./binutils-rust/bfd
$ yq eval '.package.name="bfd-sys" | .lib.name="bfd_sys"' Cargo.toml -oj | yj -jt | sponge Cargo.toml
$ cargo add f128 -Z sparse-registry && \
    cargo add zlib-sys --path=../zlib -Z sparse-registry
# 临时修复一些Rust处理不了的问题
$ sed -i 's/*mut __va_list_tag/::core::ffi::VaList/g' ./src/*.rs
$ sed -i 's|args\[i as usize\].ld = ap.arg::<f128::f128>();|args[i as usize].d = ap.arg::<libc::c_double>();|g' ./src/bfd.rs
$ cargo fix --broken-code -Z sparse-registry --allow-no-vcs
$ cargo build -Z sparse-registry

# libctf 库
$ cd `path/to/binutils-2.37`
$ ec2rust-transpile -e compile_commands_libctf.json -o ./binutils-rust/libctf
$ cd ./binutils-rust/libctf
$ yq eval '.package.name="libctf-sys" | .lib.name="libctf_sys"' Cargo.toml -oj | yj -jt | sponge Cargo.toml
$ cargo add zlib-sys --path=../zlib -Z sparse-registry
$ cargo build -Z sparse-registry

# libiberty 库
$ cd `path/to/binutils-2.37`
$ ec2rust-transpile -e compile_commands_libiberty.json -o ./binutils-rust/libiberty
$ yq eval '.package.name="libiberty-sys" | .lib.name="libiberty_sys"' Cargo.toml -oj | yj -jt | sponge Cargo.toml
$ cargo add f128 -Z sparse-registry
# 临时修复一些Rust处理不了的问题
$ sed -i '/unsafe extern "C" fn d_demangle_callback(/{n;n;n;n;n;n;n;N;N;N;N;N;N;N;N;N;N;N;N;d;}' ./src/cp_demangle.rs && \
  sed -i '/unsafe extern "C" fn d_demangle_callback(/{n;n;n;n;n;n;n;n;N;N;d;}' ./src/cp_demangle.rs && \
  sed -i -E ':a;N;$!ba;s|(let mut comps: Vec::<demangle_component>) = ::std::vec::from_elem\(\n([[:space:]]*let mut subs: Vec::<\*mut demangle_component>) = ::std::vec::from_elem\(|\1;\n\2;|g' ./src/cp_demangle.rs
$ cargo build -Z sparse-registry

# opcodes 库
$ cd `path/to/binutils-2.37`
# 这里我们无法直接使用utils-translator工具，因为opcodes库中的代码长度过大，所以我们直接使用C语言编译出来的库文件
$ cd ./opcodes
$ ar rcs libopcodes.a *.o
$ mkdir `path/to/binutils-2.37`/binutils-rust/opcodes
$ cp ./libopcodes.a `path/to/binutils-2.37`/binutils-rust/opcodes`

# ld 库
$ cd `path/to/binutils-2.37`
# 删除所有testplug相关的条目
$ jq '[.[] | select(.file | contains("testplug") | not)]' compile_commands_ld.json | sponge compile_commands_ld.json
# 转译ld库
$ ec2rust-transpile -e compile_commands_ld.json -o ./binutils-rust/ld --detect-binary --emit-binaries --emit-no-lib --fuzz-depends-level 2
$ cd ./binutils-rust/ld
# 添加依赖
$ cargo add bfd-sys --path=../bfd -Z sparse-registry && \
  cargo add libiberty-sys --path=../libiberty -Z sparse-registry && \
  cargo add libctf-sys --path=../libctf -Z sparse-registry && \
  cargo add zlib-sys --path=../zlib -Z sparse-registry
# 临时修复问题
$ sed -i 's|*mut __va_list_tag|::core::ffi::VaList|g' ./src/*.rs ./ldmain.rs && \
  sed -i -E ':a;N;$!ba;s|;\n[[:space:]]*init(\n[[:space:]]*\})|\1|g' ./src/ei386pe.rs ./src/ei386pep.rs && \
  sed -i -E 's|let mut init = ([0-9a-zA-Z_]+) \{|\1 \{|g' ./src/ei386pe.rs ./src/ei386pep.rs && \
  sed -i 's/extern crate libc;/extern crate bfd_sys;\nextern crate libctf_sys;\nextern crate libiberty_sys;\nextern crate zlib_sys;\nextern crate libc;/g' ./ldmain.rs
# 自动修复代码
$ cargo fix --broken-code -Z sparse-registry --allow-no-vcs
# 编译ld库
$ cargo build -Z sparse-registry

# binutils 二进制
$ ec2rust-transpile -e compile_commands_binutils.json -o ./binutils-rust/binutils --detect-binary --emit-binaries --emit-no-lib
$ cd ./binutils-rust/binutils
$ cargo add bfd-sys --path=../bfd -Z sparse-registry && \
    cargo add libiberty-sys --path=../libiberty -Z sparse-registry && \
    cargo add libctf-sys --path=../libctf -Z sparse-registry && \
    cargo add zlib-sys --path=../zlib -Z sparse-registry
# 添加依赖库路径到构建脚本
$ sed -i -E 's|(// add unix dependencies below)|\1\n     println!("cargo:rustc-flags=-L../opcodes -l opcodes");|g' ./build.rs
# 修复 extern crate 顺序和依赖
$ sed -i 's/extern crate libc;/extern crate bfd_sys;\nextern crate libctf_sys;\nextern crate libiberty_sys;\nextern crate zlib_sys;\nextern crate libc;/g' ./*.rs
$ cargo fix --broken-code -Z sparse-registry --allow-no-vcs
$ cargo build -Z sparse-registry

# as 库 (GNU 汇编器)
$ cd `path/to/binutils-2.37`
$ ec2rust-transpile -e compile_commands_gas.json -o ./binutils-rust/gas --detect-binary --emit-binaries --emit-no-lib
$ cd ./binutils-rust/gas
$ cargo add bfd-sys --path=../bfd -Z sparse-registry && \
    cargo add libiberty-sys --path=../libiberty -Z sparse-registry && \
    cargo add libctf-sys --path=../libctf -Z sparse-registry && \
    cargo add zlib-sys --path=../zlib -Z sparse-registry
# 添加依赖库路径到构建脚本
$ sed -i -E 's|(// add unix dependencies below)|\1\n     println!("cargo:rustc-flags=-L../opcodes -l opcodes");|g' ./build.rs
# 修复函数声明中的问题
$ sed -i -E 's/(unsafe extern "C" fn operand_type_(xor|and|or|and_not)\()/#[allow(unconditional_panic)]\n\1/g' ./src/config/tc_i386.rs && \
    sed -i '/pub type flag_code_0;/d' ./src/config/tc_i386.rs && \
    sed -i 's/flag_code_0/flag_code/g' ./src/config/tc_i386.rs
$ sed -i 's/extern crate libc;/extern crate bfd_sys;\nextern crate libctf_sys;\nextern crate libiberty_sys;\nextern crate zlib_sys;\nextern crate libc;/g' ./*.rs
$ cargo fix --broken-code -Z sparse-registry --allow-no-vcs
$ cargo build -Z sparse-registry

# gprof 库
$ cd `path/to/binutils-2.37`
$ ec2rust-transpile -e compile_commands_gprof.json -o ./binutils-rust/gprof --detect-binary --emit-binaries --emit-no-lib --fuzz-depends-level 2
$ cd ./binutils-rust/gprof
$ cargo add bfd-sys --path=../bfd -Z sparse-registry && \
    cargo add libiberty-sys --path=../libiberty -Z sparse-registry && \
    cargo add libctf-sys --path=../libctf -Z sparse-registry && \
    cargo add zlib-sys --path=../zlib -Z sparse-registry
# 添加依赖库路径到构建脚本
$ sed -i -E 's|(// add unix dependencies below)|\1\n     println!("cargo:rustc-flags=-L../opcodes -l opcodes");|g' ./build.rs
# 修复 extern crate 顺序和依赖
$ sed -i 's/extern crate libc;/extern crate bfd_sys;\nextern crate libctf_sys;\nextern crate libiberty_sys;\nextern crate zlib_sys;\nextern crate libc;/g' ./*.rs
$ cargo fix --broken-code -Z sparse-registry --allow-no-vcs
$ cargo build -Z sparse-registry
```

最后我们根据Rust的sub-crate机制，我们需要修改各个crate的Cargo.toml文件，使得他们能够正确的依赖。

具体可以直接参考我们转译好的仓库[binutils-rust](https://github.com/Licheam/binutils-rust)。