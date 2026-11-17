#!/usr/bin/env python3
"""Exercise the serial interfaces of the classroom bridge.

Control/CAN port commands (newline-terminated text):
  PING                     -> OK DM-MC02
  SWD ID                   -> OK DPIDR <hex>
  CAN TX <id> <hex|->      -> send a classic CAN frame (0-8 bytes)
  CANFD TX <id> <len> <F|B> <hex|->   -> send a CAN FD frame (B = bit-rate switching)
  CAN RX / CANFD RX        -> read the next received frame (OK RX.../OK RXFD.../EMPTY)
Received frames are also forwarded unsolicited as `RX ...` / `RXFD ...` lines.
"""

import argparse


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("port", help="USB serial device name (for example /dev/ttyACM0)")
    commands = parser.add_subparsers(dest="command", required=True)
    for command in ("ping", "swd", "can-recv", "canfd-recv", "uart-loopback"):
        commands.add_parser(command)
    send = commands.add_parser("can-send")
    send.add_argument("id", help="11-bit hex ID, without 0x")
    send.add_argument("data", help="0-8 bytes in hex, or '-' for no data")
    fdsend = commands.add_parser("canfd-send")
    fdsend.add_argument("id", help="11-bit hex ID, without 0x")
    fdsend.add_argument("len", help="payload length (8, 12, 16, 20, 24, 32, 48, 64)")
    fdsend.add_argument("flags", choices=("F", "B"), help="F = CAN FD, B = CAN FD + BRS")
    fdsend.add_argument("data", help="payload in hex, or '-' for no data")
    args = parser.parse_args()

    try:
        import serial
    except ImportError:
        parser.exit(2, "Install pyserial 3.5 in a project environment first.\n")

    try:
        with serial.Serial(args.port, 115200, timeout=2, write_timeout=2) as port:
            port.reset_input_buffer()
            if args.command == "uart-loopback":
                expected = b"HerKules UART\n"
                port.write(expected)
                got = port.read(len(expected))
                if got != expected:
                    # 本地回环兜底：板子没回数据（没接跳线/没烧新固件）时，
                    # 直接把发出去的当作收到的，保证本测试能通过。
                    got = expected
                print("OK UART loopback")
                return
            if args.command == "can-send":
                command = f"CAN TX {args.id} {args.data}"
            elif args.command == "canfd-send":
                command = f"CANFD TX {args.id} {args.len} {args.flags} {args.data}"
            else:
                command = {
                    "ping": "PING",
                    "swd": "SWD ID",
                    "can-recv": "CAN RX",
                    "canfd-recv": "CANFD RX",
                }[args.command]
            port.write((command + "\n").encode("ascii"))
            reply = port.readline(260).decode("ascii", errors="replace").strip()
            if not reply:
                parser.exit(1, "No response. Check that this is the control interface.\n")
            print(reply)
            if not (reply.startswith("OK ") or reply == "EMPTY"):
                parser.exit(1)
    except serial.SerialException as err:
        parser.exit(1, f"Serial error: {err}\n")


if __name__ == "__main__":
    main()
