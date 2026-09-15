#!/usr/bin/env python3
"""PyMySQL client demo: chaoticdb 兼容 MySQL wire 协议的一部分。

chaoticdb 自己实现了握手 / COM_QUERY / COM_STMT_PREPARE 等协议子集，
因此可以直接用原生的 PyMySQL 驱动连接，无需任何适配层。

前置：在 config.toml 中打开 MySQL 监听，例如

    [server]
    mysql_addr = "127.0.0.1:13306"

启动服务：

    target\\debug\\chaoticdb.exe serve .\\data

运行本脚本：

    python showcase\\pymysql_client.py

演示时改下面 HOST/PORT 与 SQL 即可。
"""

import pymysql

HOST = "127.0.0.1"
PORT = 3306
USER = "root"
PASSWORD = ""
DATABASE = None  # 指定默认数据库，如 "main"；留 None 表示不选库

SQL = "SELECT * FROM STUDENT"


def main() -> None:
    conn = pymysql.connect(
        host=HOST,
        port=PORT,
        user=USER,
        password=PASSWORD or None,
        database=DATABASE,
        charset="utf8mb4",
        autocommit=True,
    )
    try:
        with conn.cursor() as cur:
            # 在这里替换/追加语句来演示协议兼容性
            cur.execute(SQL)

            if cur.description:
                print(" | ".join(column[0] for column in cur.description))
                for row in cur.fetchall():
                    print(" | ".join("NULL" if v is None else str(v) for v in row))
            else:
                print(f"{cur.rowcount} row(s) affected")
    finally:
        conn.close()


if __name__ == "__main__":
    main()
