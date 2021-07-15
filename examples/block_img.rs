#![cfg(all(any(target_arch = "x86", target_arch = "x86_64"), target_feature = "avx2"))]

use block_aligner::scan_block::*;
use block_aligner::scores::*;
use block_aligner::cigar::*;

use image::{Rgb, RgbImage};
use imageproc::drawing::*;
use imageproc::rect::Rect;

use std::env;

fn main() {
    let mut args = env::args().skip(1);
    let img_path = args.next().unwrap();
    //let q = b"MKEKIKKNILEKVISKFEFPLNMKRIGIDIGSDNLKAVVIDGKNITFYQKKINGKPNYASKEILEKIAEKHGDEAKIAITGVNSFSLSDILKKINEPTAIKEGISSLDLGIKKKEKLVVIDAGASSLEYFEFERHNGKLFLENYNLEDKCGGGSGLLLDHMAKRFNYDSIEEFSNVANQTEKTIKLSAKCGVFRESDVVHQQQKGTPKEVLAASLYRASADSFKNILSNGIIPEGKIVLIGGLSLSKAFVKHLINVCKISSERVIIPKQGLHIGAIGAAIYGQQVYLNNIIKKLEQKLTKPFNYESQGPLILKKSKIMKPKEDWPYGADIPLAGLGVDIGSVSTKAALIAEINGKFRLLAYYYRRTEIDPVGAAIDVINKVYNQVIERGYKIEKVVAGTTGSGRQLTGFIVGASKEHIVDEITAQAAGITTVYPQKEFSIVEFGGQDSKFINISQGVVVGFEMNNACAAGTGALLEKYAMRRDIKIEDFGDIALRAKKPPDIDSTCAVLSEQSIIKYEQNNVSLEDLCAALTLATARNYLAKVVRGKEIKEKVVFQGATAFNLGQVAALETVLERGIVVPPWPHITGAIGAAKYAYDIKRLGDFRGFKSISNLKYNVGPFECINKGCGNDCNITMAKIGDEEFYIGDRCQRYSAKKAEKKIKPPNLFKERQKIMEDACK";
    //let r = b"MKEKNKENILENVISKFRFPLNMKRIGIDIGSDNLKAVVIDGKDITTYIKKINGKPIYALKETLDEIITKNGNEAYLGVTGVNSISLSDVLNEKQVLSESITTKGGIAFLDLDIKENEEFAVIDIGASNQRYYEFGKDKNSGKLILEHNCLQDKCGAGSGLLLEHMAKRFEYGSIEELSNVANQTEKTIKLSAKCGVFRESDVVHQQQKGTPKEVLAASLYRASADSFKTILSNGMIPEGRIILIGGLSLSKAFVKHLIDVCKISSERVIIPKQGLHIVAIGAAIYGQQVYLNNIIKKLEQKLTKPFNYESQGPLILKKSIIMKPKEDWPYGADVPLAGLGIDIGSVSTKAALIAKINGKFRLLAYHYRRTEIDPVGAAIDVINKVYNQVIERGYKIEKVVAGTTGSGRQLTGFIVGASKEHIVDEITAQAAGITTVYPQKEFSIIEFGGQDSKFINISQGVVVDFAMNNACAAGTGALLEKYAMRRGIKIEDFGDIALRAKNPPDIDSTCAVLSEQSIMKYEQNNVSLEDLCAAITLATARNYLAKVVSGSEIKEKVVFQGATAFNLGQVAALETVLGKGIIVPPWPHITGAIGAAKYAYDTSNLGGFREFKKILNLKYNVGPYECINKGCGNDCNITMAKIGDEEFYIGDRCQRYSAKKDEKKIKPPNLFKERQKIMEDACQ";
    //let q = b"MSGQLRFEILGPQRAWYADREIDLGPGKQRAVLAVLLLAPGRPVSTGQIIEAVWPEDPPANGPNVVQKYVAGLRRVLEPDRSPRTPGQVLSLTDAGYLLRVDPEAVDAIRFERGVQAARRHASESPEEALAELTDALARWHGEPFTGFSGGVFEAARHRLVELRAVALETRAELELDSGRHRETVGELVELVAEFPVRERLRHQYMLALYRSGRQAEALAAYRDIDSLLREEHGIGPGEALRELHARILRADPTLTAGVPNAGRPAPAVPVQATVPAPAPVPPAPAPVPHYADLPSPLQPARDGLPAGSPSPFAVEQPRRERHPLPTWVSTTTTVLGTLLALVSFGTGTWLVVLAYAVRRRSRRLGLAALGYFLTATLLVVMIATDDLETEDSPAEVFSGLGVIFLCWLVGSAHIVLLNSWVWGRITGRPRVAQALEEERRIRRQQARNLLHRYPSARLEFAIGRPDLPRAFDDGGLVDVNAVPDQVLATLPGLTDAQRRQVAMDRWVRGPYGSMEELAARCPLPVAATEALRDVLLFLPVHTPVTTRSERFGPR";
    //let r = b"MSGQLRFEILGPQRAWYADREIDLGPGKQRAVLAVLLLSAGRPVSTGQIVDAVWPEEPPANGPNVVQKYVAGLRRVLEPDRSPRTPGQVLSLTDAGYLLRVDPEAVDAVRFERGVQAARQQHAAGRTDEALAELATALDLWQGEPFTGFTGPIFEAARQRLVELRAVALETRAELALDLGRHRETVGELVELVAEFPVRERLRQQLMLALYRSGRQAEALAAYREFGDLLREEYGIEPGEALRVLHGRILRADPTLVVAKPATEPAAPPPATVGSVAVVPAQERAADPVDPADPVSAVPPPAPPPAPPSTAFPPMPRGQYPVHGTPPRRARRWVSISTAIVAALIGLLSFGLLTWLVVLVYAVWRRSWRHGLAALGYLSAMAVFIGVVGSGDPDEVTTVDFVAIIAWLFSWLLGVLHTLLLNGAIWSAITGRGTAATGPVAHERRVRREQARYLLYHYPAARQELHIGRPDLPRTFDDGGLVDVNAVPEATIAELPGITAEQARQVVVDRWLRGPFGSMEDLAARCLLPLLLTDSLRDHLVFLPPMPGPSTAIPQQAAARQQTGGDVRPTW";
    let q = b"MPITQIENLMEESPPIHDITQDTPSTDAVKPYLRGLNLYYAAISAILGAGVLFMHVGIADGGICAWIMAVLLCGISGTVVSARTYRMAIDMLRATGKEEGYQLLPVTANFLRIPTDETDPERQHVKKYATTQVEAAPHNASPNPPRPSLTFGSLVLDEHKAVRICLNLAIVSLSAGSLIVYLGLMYQWVWDIVDVLRAHTPFGVFPSLTKWGSFGGLVLVCMYLAYLPTPGDNKIITTISAISLMLLTIMVLAMLLIVRYSSSFAITKCITFSSEAWKGMVVGRHFSFLKFTGAVATIFFAINSQQNMPVYFSLAKPKSKKAFFKIFSIAMMVASSLFLVVGVCGYFITFASSSNYNLKLDNILTNIGTILEHVRPLLTSALFKTFLLLTTGIKVLMIPVLLSAFMWQCAAAKNVLINVFSGYFKNYKAKSIFRKAIGPALAALALIPVFLNVDLGFIIRVLGSGSGAYIILFIPAWIVLRGTHSKDGESTWLHYMGAAFMGLGFLVLIAYTVIDSTPLRRIFETPLPATPKQLPICFPDFD";
    let r = b"MTIITNQDFVDEIPDTANEHEIPDTAAGKPQLRFVNQYYVVASATFGAGALFMHAGIANGGVCAWIVAVVLYAISGTLVTTQTYRLYIGMLQGTEKEEECELLSTKDRSHYVAAKEADPETEAGAKFVAIQTNGGPHNPNTNPNTSPNTHRPSLTFGSFVLDEFMAVRIGLALAIFSLSFGTMMVYMGLIYRTVRGLITLCERGSSIPEWLPVVAKWFWFGAYFMVYAYLAYVSPISEGVGIVMFSAIALVMLTAIVAILFLIVRYSPTFASTRCLTFSTDAWKGMVVGRQFSFLKFTGAIATIFYAINSHLNMPVYLSLVKPKSKKVQFTTFSAANMVVCGLCLIIGVCGYSISFTDSLRGLKLDTILTTIERTLESLNGVTQNHGHNDGPIIYLLSIYINLSTSLLLLLPFMWQAAAAKSVLISVFGGHIKDHKTKNIFKKAIGPALALLTLIPTLLMVDLGFITRVLGSSAGVYIVLFLPAWIILRGTHTKDSESTWLHYTGAAFMGLGFLALIAYTVIVSTPLHRLVVTVPPQPWLQPSC";
    let r_padded = PaddedBytes::from_bytes::<AAMatrix>(r, 2048);
    let q_padded = PaddedBytes::from_bytes::<AAMatrix>(q, 2048);
    let run_gaps = Gaps { open: -11, extend: -1 };

    let block_aligner = Block::<_, true, false>::align(&q_padded, &r_padded, &BLOSUM62, run_gaps, 32..=256, 0);
    let blocks = block_aligner.trace().blocks();
    let cigar = block_aligner.trace().cigar(q.len(), r.len());

    let cell_size = 3;
    let img_width = ((r.len() + 1) * cell_size) as u32;
    let img_height = ((q.len() + 1) * cell_size) as u32;
    let white = Rgb([255u8, 255u8, 255u8]);
    let blue = Rgb([0u8, 0u8, 255u8]);
    let red = Rgb([255u8, 0u8, 0u8]);
    let mut img = RgbImage::new(img_width, img_height);
    println!("path: {}", img_path);
    println!("img size: {} x {}", img_width, img_height);

    draw_filled_rect_mut(&mut img, Rect::at(0, 0).of_size(img_width, img_height), white);

    for block in &blocks {
        if block.width == 0 || block.height == 0 { continue; }
        let x = (block.col * cell_size) as i32;
        let y = (block.row * cell_size) as i32;
        let width = (block.width * cell_size) as u32;
        let height = (block.height * cell_size) as u32;

        draw_filled_rect_mut(&mut img, Rect::at(x, y).of_size(width, height), blue);
        draw_hollow_rect_mut(&mut img, Rect::at(x, y).of_size(width, height), white);
    }

    let mut x = cell_size / 2;
    let mut y = cell_size / 2;
    let vec = cigar.to_vec();

    for op_len in &vec {
        let (next_x, next_y) = match op_len.op {
            Operation::M => (x + op_len.len * cell_size, y + op_len.len * cell_size),
            Operation::I => (x, y + op_len.len * cell_size),
            _ => (x + op_len.len * cell_size, y)
        };
        draw_line_segment_mut(&mut img, (x as f32, y as f32), (next_x as f32, next_y as f32), red);
        x = next_x;
        y = next_y;
    }

    img.save(img_path).unwrap();
}
