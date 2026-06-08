# GOGExtract-RS
gogextract.py rewrite in Rust with enhancements

- Progress bar
- File decompression
- Multithread performance
<br>
A few notable differences in operation

- Only extracts\decompresses game data not the the install script or mojosetup files
- Extracts into a temp folder first then moves into a folder named after the installer






# Orginal GOG Extract Script

Script for unpacking GOG Linux installers.

Explanation of how it works is in my [blog post].

## Usage

`gogextract.py <input file> <output dir>`

Output files will be named `unpacker.sh`, `mojosetup.tar.gz` and `data.zip`.

## License

[MIT](LICENSE)

[blog post]: https://yepoleb.github.io/blog/2016/10/09/how-the-gog-linux-installers-work/
