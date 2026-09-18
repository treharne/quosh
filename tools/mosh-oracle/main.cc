// Differential oracle for quosh-predict: drives Mosh's unmodified
// PredictionEngine from a scripted command stream and dumps the rendered
// framebuffer. `tests/oracle.rs` replays the same stream through the Rust
// predictor and compares.
//
// Built by `build.sh`; not part of the Quosh build. See tools/mosh-oracle/README.md.

#include <clocale>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <iostream>
#include <sstream>
#include <string>
#include <vector>

#include "src/frontend/terminaloverlay.h"
#include "src/terminal/terminal.h"

using namespace Terminal;
using namespace Overlay;

static uint64_t g_now = 0;
uint64_t timestamp( void ) { return g_now; }

static void feed( Emulator& emu, const std::string& s ) {
  Parser::UTF8Parser parser;
  Parser::Actions actions;
  for ( unsigned char b : s ) {
    parser.input( (char)b, actions );
    for ( auto& a : actions ) {
      a->act_on_terminal( &emu );
    }
    actions.clear();
  }
}

static std::string render_row( const Framebuffer& fb, int row, std::string& ul ) {
  std::string out;
  ul.clear();
  for ( int c = 0; c < fb.ds.get_width(); c++ ) {
    const Cell* cell = fb.get_cell( row, c );
    cell->print_grapheme( out );
    ul += cell->get_renditions().get_attribute( Renditions::underlined ) ? '1' : '0';
  }
  return out;
}

static void dump( const Framebuffer& fb ) {
  printf( "CUR %d %d\n", fb.ds.get_cursor_row(), fb.ds.get_cursor_col() );
  for ( int r = 0; r < fb.ds.get_height(); r++ ) {
    std::string ul;
    std::string row = render_row( fb, r, ul );
    printf( "ROW %d %s\n", r, row.c_str() );
    printf( "UL  %d %s\n", r, ul.c_str() );
  }
  printf( "END\n" );
}

static std::vector<unsigned char> unhex( const std::string& s ) {
  std::vector<unsigned char> v;
  for ( size_t i = 0; i + 1 < s.size(); i += 2 ) {
    v.push_back( (unsigned char)strtol( s.substr( i, 2 ).c_str(), nullptr, 16 ) );
  }
  return v;
}

static void render( const Emulator& emu, PredictionEngine& pred, Framebuffer& local ) {
  Framebuffer shown = emu.get_fb();
  pred.cull( shown );
  pred.apply( shown );
  dump( shown );
  local = shown;
}

int main( int argc, char** argv ) {
  setlocale( LC_ALL, "" );
  int w = 80, h = 24;
  if ( argc >= 3 ) {
    w = atoi( argv[1] );
    h = atoi( argv[2] );
  }
  Emulator emu( w, h );
  PredictionEngine pred;
  pred.set_display_preference( PredictionEngine::Adaptive );
  pred.set_send_interval( 250 );

  Framebuffer local = emu.get_fb();
  uint64_t sent = 0;

  std::string line;
  while ( std::getline( std::cin, line ) ) {
    std::istringstream is( line );
    std::string cmd;
    is >> cmd;
    if ( cmd == "KEY" ) {
      std::string hex;
      is >> hex;
      auto bytes = unhex( hex );
      for ( auto b : bytes ) {
        pred.set_local_frame_sent( sent );
        sent++;
        pred.new_user_byte( (char)b, local );
      }
      render( emu, pred, local );
    } else if ( cmd == "FEED" ) {
      std::string hex;
      is >> hex;
      auto bytes = unhex( hex );
      feed( emu, std::string( bytes.begin(), bytes.end() ) );
      render( emu, pred, local );
    } else if ( cmd == "SENT" ) {
      is >> sent;
      pred.set_local_frame_sent( sent );
    } else if ( cmd == "EARLY" ) {
      uint64_t n;
      is >> n;
      pred.set_local_frame_acked( n );
    } else if ( cmd == "LATE" ) {
      uint64_t n;
      is >> n;
      pred.set_local_frame_late_acked( n );
    } else if ( cmd == "RTT" ) {
      unsigned int n;
      is >> n;
      pred.set_send_interval( n );
    } else if ( cmd == "TICK" ) {
      uint64_t n;
      is >> n;
      g_now += n;
      render( emu, pred, local );
    } else if ( cmd == "RESET" ) {
      pred.reset();
    } else if ( cmd == "DUMP" ) {
      dump( local );
    } else if ( cmd == "PRED" ) {
      std::string what;
      is >> what;
      if ( what == "always" ) {
        pred.set_display_preference( PredictionEngine::Always );
      } else if ( what == "never" ) {
        pred.set_display_preference( PredictionEngine::Never );
      } else {
        pred.set_display_preference( PredictionEngine::Adaptive );
      }
    }
  }
  return 0;
}
