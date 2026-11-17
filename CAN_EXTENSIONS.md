# CAN 扩展任务实现说明（Classical CAN / CAN FD / SocketCAN）

本说明对应 DM-MC02 作业的**选做拓展**三选任务，三份任务都已在同一份固件里实现：

1. **Classical CAN** —— 与主机双向桥接（主动转发 + 命令发送）。
2. **CAN FD（5 Mbit/s + TDC）** —— 仲裁段 1 Mbit/s、数据段 5 Mbit/s、启用 BRS 与 TDC。
3. **Linux SocketCAN** —— 设备端实现 `gs_usb` 协议，Linux 将板卡识别为 `can0`。

> 状态：代码已在本机以 `cargo build --locked` 编译通过并生成固件 ELF；**尚未上板验证**，
> 下述硬件行为按预期描述，上板后需按对端实际配置核对。

---

## 0. 总体结构

一份固件里三个功能共享**同一个板载 FDCAN1**。由于 CAN FD 是 Classic CAN 的超集
（一个配置成 `AllowFdCanAndBRS` 的节点仍能收发经典帧），固件默认使用
`board::can1_fd()` 初始化 FDCAN1：仲裁段 1 Mbit/s、数据段 5 Mbit/s、BRS + TDC。
经典帧在此配置下照常以经典格式收发，因此三个任务共用这一套初始化。

FDCAN1 被封装成**有界缓冲的读写句柄**（`embassy_stm32::can` 的 `buffered_fd`）：

- `wiring::Ports { can_tx, can_rx, .. }`：`can_tx` / `can_rx` 是可克隆的发送/接收句柄，
  分别交给**控制串口任务**（任务 1、2）和 **gs_usb 任务**（任务 3）。

```text
                     ┌──────────────────────────────────────────────┐
                     │                  FDCAN1 (CAN FD + BRS + TDC)  │
                     │       PD0=RX  PD1=TX  -> MCP2542FD -> CAN_H/L │
                     └───────────────┬──────────────────────────────┘
                                     │ buffered_fd (TxFdBuf/RxFdBuf, 各 32)
              ┌──────────────────────┴──────────────────────┐
              │ can_tx / can_rx (clone)                     │ can_tx / can_rx (clone)
              ▼                                             ▼
    control_port (CDC 控制串口)                     gs_usb (bulk IN/OUT)
     PING / SWD ID / CAN TX / CANFD TX             Linux gs_usb 驱动 -> can0
     主动转发 RX... / RXFD...                      cansend / candump
```

涉及文件：

| 文件 | 作用 |
| --- | --- |
| `src/board.rs` | `can1()`（原有 Classic 配置，保留）与 `can1_fd()`（CAN FD + BRS + TDC，含 TDCR 寄存器写入） |
| `src/wiring.rs` | 用 `buffered_fd` 把 FDCAN1 变成可共享的 `can_tx`/`can_rx` |
| `src/protocol.rs` | 控制串口的文本命令协议、经典/FD 帧解析、转发格式化 |
| `src/gs_usb.rs` | `gs_usb` 协议：USB 描述符、控制请求处理、帧双向转换 |
| `src/main.rs` | 任务编排：USB 设备 + 控制串口 + UART 串口 + DAP + gs_usb |
| `host.py` | 主机侧脚本（`ping/swd/can-send/canfd-send/can-recv/canfd-recv`） |

---

## 1. 编译验收

- 工具链：`rust-toolchain.toml` 固定 `1.99.1`，目标 `thumbv7em-none-eabihf`（Cortex-M7）。
- 命令：

```sh
cargo build --locked
```

- 结果：编译 + 链接通过，产物为
  `target/thumbv7em-none-eabihf/debug/dm-mc02-cmsis-dap`（无扩展名 ELF 固件）。
  完整成功日志见 `build-success.log`。
- 主机脚本语法检查：

```sh
python3 -m py_compile host.py
```

- 依赖改动：在 `Cargo.toml` 中新增了 `stm32-metapac = "=21.0.0"`（用于在配置模式下直接写
  FDCAN 的 TDCR 寄存器，实现 TDC），并将 `embassy-usb` 的接口数上限从
  `max-interface-count-5` 提到 `max-interface-count-6`（新增一个 gs_usb 接口）。
  `Cargo.lock` 已同步更新。

---

## 2. 任务 1 —— Classical CAN 与主机双向桥接

### 数据路径

- **CAN → 主机**：FDCAN 收到帧 → 中断把帧放入 `RxFdBuf`（有界）→ 控制串口任务在
  `select(主机读取, can_rx.receive())` 中收到帧 → `protocol::format_frame` 序列化成一行
  `RX ...` / `RXFD ...` → 通过 CDC 数据端点 `write_all` 写回主机（**主动转发**，无需主机轮询）。
- **主机 → CAN**：主机发 `CAN TX ...` → 任务 `read_packet` 按字节累积到 `Line`，遇 `\n`
  出命令 → `protocol::execute` 解析成 `FdFrame` → `can_tx.write` 送入 `TxFdBuf`（有界）→
  中断把帧写进 FDCAN 发送邮箱 → 上总线。

### 主机协议（文本，换行符为帧边界）

一行一条命令或一条转发消息，字段以空白分隔：

| 方向 | 格式 | 说明 |
| --- | --- | --- |
| 主机→板 | `CAN TX <id:3hex> <data:hex\|->` | 经典数据帧，0–8 字节 |
| 主机→板 | `CANFD TX <id:3hex> <len> <F\|B> <data:hex\|->` | CAN FD，`F`=FD、`B`=FD+BRS，len∈{0..8,12,16,20,24,32,48,64} |
| 主机→板 | `CAN RX` / `CANFD RX` | 阻塞读一帧（100 ms 超时） |
| 板→主机 | `RX <id:3hex> <len> <data:hex\|->` | 收到经典帧（主动转发） |
| 板→主机 | `RXFD <id:3hex> <len> <F\|B> <data:hex\|->` | 收到 FD 帧（主动转发） |
| 命令应答 | 同形状，前缀 `OK ` | `EMPTY` = 超时无帧 |

示例：

```text
# 发送：11 位 ID 0x321，载荷 A5 5A
CAN TX 321 A55A
# 发送：CAN FD，ID 0x123，12 字节，带 BRS
CANFD TX 123 12 B 00112233445566778899AABB
# 收到经典帧（主动转发，无需请求）
RX 321 2 A55A
# 收到 FD 帧（带 BRS）
RXFD 123 12 B 00112233445566778899AABB
```

### 帧边界识别

`Line` 以 `\n` 为边界逐字节累积：一条请求跨多个 USB 包到达时，未遇到 `\n` 前一直保存在
`Line` 缓冲里；一个 USB 包内含多条命令时，逐个 `\n` 切开、逐条执行。缓冲满（200 字节）
仍未换行的行被丢弃，避免截断命令被误执行。

### 并发与缓冲

- FDCAN 硬件本身有界：3 个发送邮箱、2 个 3 深的接收 FIFO。
- 软件再加一层有界队列：`TxFdBuf<32>` / `RxFdBuf<32>`（`embassy_sync::Channel`）。
- 队列满：发送侧 `can_tx.write` 有背压（等待队列空位），命令路径用 `with_timeout(100ms)`
  包裹，超时返回 `ERR CAN TX`，不会永久阻塞控制口；接收侧 `Channel` 满时 ISR 里
  `try_send` 丢弃新帧（`rx_mode.on_interrupt` 里 `let _ = try_send`），表现为丢帧而非阻塞。
- 主机断开：CDC 写失败（`write_all` 报错）即退出当前 `'connection` 循环，回到
  `wait_connection` 等待重连；期间 FDCAN 继续按中断接收，队列满则丢帧。
- 任务分工：`control_port` 用 `select` 同时监听“主机读”与“CAN 读”两路，任意一路不会独占
  CPU；CAN 收发由 FDCAN 的 IT0/IT1 中断驱动，USB 由 OTG_HS 中断驱动。

---

## 3. 任务 2 —— 5 Mbit/s CAN FD 与 TDC

### 位时序计算（FDCAN 内核时钟 80 MHz）

**仲裁段（标称位率）1 Mbit/s，采样点 75%**

- `prescaler = 1` → 一个时间量子 tq = 12.5 ns。
- `seg1 = 59`、`seg2 = 20`、同步段 = 1 tq。
- 位时间 = 1 + 59 + 20 = 80 tq = 80 × 12.5 ns = **1000 ns = 1 Mbit/s**。
- 采样点 = (1 + 59) / 80 = **75%**；SJW = 20 tq。

**数据段（BRS 后可变位率）5 Mbit/s，采样点 75%**

- `prescaler = 1` → tq = 12.5 ns。
- `seg1 = 11`、`seg2 = 4`、同步段 = 1 tq。
- 位时间 = 1 + 11 + 4 = 16 tq = 16 × 12.5 ns = **200 ns = 5 Mbit/s**。
- 采样点 = (1 + 11) / 16 = **75%**；SJW = 4 tq。

对应代码 `src/board.rs::can1_fd()` 里的 `NominalBitTiming` 与 `DataBitTiming`，
并通过 `.set_frame_transmit(FrameTransmissionConfig::AllowFdCanAndBRS)` 打开 BRS。

### TDC（发射延迟补偿）

- **寄存器**：FDCAN 的 `TDCR`，字段 `TDCO[6:0]`（偏移）、`TDCF[6:0]`（滤波窗口长度）。
- **单位**：mtq（minimum time quantum）= 1 个 FDCAN 内核时钟周期 = 1 / 80 MHz = 12.5 ns。
- **设置**：`TDCO = 10`（= 125 ns）、`TDCF = 0`。
- **依据**：数据段 BRS 下，发送节点需要在“二次采样点 SSP”上监测自己发出的位；SSP 位置 =
  实测 TX→RX 环路延迟 + TDCO。板载收发器 MCP2542FD 的 TX→RX 环路延迟典型约 100–120 ns，
  取 `TDCO = 10 mtq ≈ 125 ns` 覆盖该延迟；TDCF = 0 表示不做滤波窗口（环路延迟近似固定）。
- **代码落点**：`src/board.rs::can1_fd()` 在 `CanConfigurator` 仍处于配置模式
  （CCE=1、INIT=1）时，通过 `stm32_metapac::FDCAN1.tdcr()` 直接写入。Embassy 0.6.0 的
  FDCAN 驱动只保存了 `DataBitTiming::transceiver_delay_compensation` 开关但并未真正写 TDCR，
  因此这里补上了寄存器写入。

### FD 帧处理

- 收发统一走 `FdFrame` / `FdEnvelope`（`embassy_stm32::can::frame`）。`Header` 里的
  `fdcan()`、`bit_rate_switching()`、`len()` 分别对应 FDF、BRS、实际数据长度。
- 发送：`CANFD TX` 用 `Header::new_fd(id, len, rtr=false, brs)` 构造；`len` 只接受
  FD 合法长度 `{0..8, 12, 16, 20, 24, 32, 48, 64}`。
- 接收：消息 RAM 的 RX FIFO 元素尺寸在驱动里已配成 64 字节
  （`configure_msg_ram` 里 `rxesc.fds = 0b111`），因此 12/32/64 字节长帧可直接进入收发缓冲；
  `extract_frame` 按 `fdf`/`brs` 位还原 `Header`。
- 主机命令与转发都用 `RXFD <id> <len> <F|B> <hex>` 表示，`B` 即 BRS。
- 上板时对端需匹配：同为 ISO CAN FD、仲裁段 1 Mbit/s、数据段 5 Mbit/s、BRS 打开、
  TDC 按对端收发器延迟设置。

---

## 4. 任务 3 —— Linux SocketCAN（gs_usb）

### 协议选择与来源

采用 **gs_usb**（candleLight / CANtact 类 USB-CAN 适配器协议），Linux 主线驱动为
`drivers/net/can/usb/gs_usb.c`。协议定义参考
[torvalds/linux `gs_usb.c`](https://github.com/torvalds/linux/blob/master/drivers/net/can/usb/gs_usb.c)。

设备侧实现（`src/gs_usb.rs`）：

- **USB 接口**：一个厂商类接口（class 0xFF），一个 bulk IN（板→主机）与一个 bulk OUT
  （主机→板）。全速 bulk 包 64 字节。
- **控制请求**（vendor + interface）：

| bRequest | 方向 | 实现内容 |
| --- | --- | --- |
| `HOST_FORMAT` (0) | OUT | 接受主机字节序配置 |
| `BITTIMING` (1) / `DATA_BITTIMING` (11) | OUT | 接受位时序（板固定 1 Mbit/s 标称段） |
| `MODE` (2) | OUT | `START`/`RESET`，置/清 `STARTED` 标志 |
| `BT_CONST` (4) | IN | 上报 fclk_can=80 MHz、经典 CAN 位时序常数、feature=0 |
| `DEVICE_CONFIG` (5) | IN | 上报单通道、固件版本 |
| `GET_STATE` (15) | IN | 上报 Error Active 与收发错误计数 |

### 帧转换

- **主机 → CAN**：bulk OUT 收 `gs_host_frame`（经典 20 字节：echo_id、can_id、can_dlc、
  channel、flags、data[8]）→ 按 `CAN_EFF_FLAG` 区分扩展/标准 ID → 构造成 `FdFrame`
  （经典头）→ `can_tx.write`。发送后把原帧按原 `echo_id` 回写 bulk IN（发送回显）。
- **CAN → 主机**：`can_rx.receive` 收到 `FdEnvelope` → 仅取经典帧（`fdcan()==false`）→
  组装 `gs_host_frame`（`echo_id=0xFFFFFFFF`、`can_id`、`can_dlc`、data）→ 写 bulk IN。
- **启动/停止**：Linux `ip link set can0 up` 会先发 `BITTIMING` 再发 `MODE START`；
  固件用 `STARTED` 门控帧转发，未启动时只排空 bulk OUT、不抢占 CAN 接收（把接收帧留给
  控制串口桥接路径）。队列满/断连：发送 `with_timeout` 包裹避免阻塞，断连后回到端点
  `wait_enabled` 等重连。

### Linux 用法

设备 VID:PID 为课堂用 `0xC0DE:0xCAFC`（非已分配 ID），需要把 `gs_usb` 驱动绑定到它：

```sh
sudo modprobe gs_usb
echo "c0de cafc" | sudo tee /sys/bus/usb/drivers/gs_usb/new_id
```

（也可写成 udev 规则；或把固件 VID:PID 改成 candleLight 的 `0x1209:0x2323` 直接免 new_id。）

随后配置并收发：

```sh
sudo ip link set can0 up type can bitrate 1000000
candump can0
cansend can0 321#A55A
```

预期 `ip link` 出现一个 `can0`（`gs_usb` 驱动的 SocketCAN 接口），`candump` 打印板子收到的
总线帧，`cansend` 经 bulk OUT → FDCAN1 → 收发器上总线，并由另一端 `candump` 收到。

---

## 5. 上板验证步骤（预期，尚未上板验证）

1. `cargo build --locked` 出固件，`cargo run --locked` 经外置探针烧录（见模板 README 第 4 节）。
2. 供电后确认出现 CMSIS-DAP 演示接口、两个 CDC 串口、以及 gs_usb 厂商接口。
3. **任务 1**：两个节点 H-H、L-L 接线并在主干两端加 120Ω 终端；本机 `host.py <控制口> can-send 321 A55A`，
   对端（或对端 `host.py`）应主动看到 `RX 321 2 A55A`。
4. **任务 2**：对端同样开启 CAN FD + BRS（仲裁 1 Mbit/s、数据 5 Mbit/s）；本机
   `host.py <控制口> canfd-send 123 12 B 00112233445566778899AABB`，对端应收到
   `RXFD 123 12 B ...`；再反向验证 12 字节双向。
5. **任务 3**：绑定 `gs_usb` 后 `ip link set can0 up type can bitrate 1000000`，
   `candump can0` 与 `cansend can0 321#A55A` 验证收发。

### 已知限制（说明）

- 三个功能共享 FDCAN1，控制串口桥接与 gs_usb 是两个**互斥的主机接口**：同时使用两条
  路径时，接收帧会被两条路径按“先到先得”瓜分，正常验证时一次只用其一。
- gs_usb 当前按基础范围实现经典 CAN 标准/扩展数据帧；FD 帧转发需在 BT_CONST 中置
  `GS_CAN_FEATURE_FD` 并实现 `canfd` 包（76 字节、跨两个全速包），可后续扩展。
- 发送回显在帧入队后立即返回（非严格“上总线完成”），不影响功能验证。
