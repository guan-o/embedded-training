CAN 扩展任务提交说明（任务 1 + 任务 2）

任务1：classic CAN 双向桥接

入口：main.rs::control_port 用 select 同时监听「主机 USB 读」和「CAN 接收」两路。

CAN>主机：FDCAN 中断把帧放入 RxFdBuf(32) > can_rx.receive() 收到 > protocol::format_frame 格式化为 RX <id> <len> <hex> > usb_tx.write_all 主动写回主机（无需主机轮询）。

主机>CAN：主机发CAN TX <id> <hex> > Line 按 \n 切分命令 → protocol::execute 解析成 FdFrame > can_tx.write 送入 TxFdBuf(32) > PDU 写进发送邮箱 > 上总线。

缓冲：TxFdBuf/RxFdBuf 各 32 深；发送用 with_timeout(100ms) 防队列满时阻塞控制口；USB 断连则退出当前连接循环等待重连。

任务2：5 Mbit/s CAN FD + BRS + TDC

入口：board.rs::can1_fd()

位时序（FDCAN 内核 80 MHz）：
 仲裁段 1 Mbit/s：prescaler=1, seg1=59, seg2=20, SJW=20（80 tq，采样点 75%）
 数据段 5 Mbit/s：prescaler=1, seg1=11, seg2=4, SJW=4（16 tq，采样点 75%）
 set_frame_transmit(AllowFdCanAndBRS) 打开 BRS

TDC:Embassy 驱动不写 TDCR，所以在配置模式下直接用 stm32_metapac::FDCAN1.tdcr() 写入 TDCO=10（10 mtq ≈ 125 ns，覆盖 MCP2542FD TX→RX 环路延迟）、TDCF=0

帧处理：CANFD TX <id> <len> <F|B> <hex> 用 Header::new_fd 构造 FdFrame，支持 12/32/64 字节；接收转发为 RXFD …。消息 RAM 元素已按 64 字节配置（驱动默认）
 
